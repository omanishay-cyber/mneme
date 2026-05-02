//! `mneme cache` — first-launch UX. Disk-usage management for the
//! per-project shards, snapshots, and cache directories under
//! `~/.mneme/`.
//!
//! NEW-058: without these subcommands, users on small drives have no
//! recovery path — `~/.mneme/projects/<id>/` grows monotonically with
//! each indexed project, snapshots accumulate forever, and the only
//! "fix" is a hand-rolled `Remove-Item -Recurse ~/.mneme/`. That is
//! NOT acceptable as a basic operational command — established 2026-04-25.
//!
//! Subcommands:
//!   `mneme cache du`              disk-usage breakdown (human or `--json`)
//!   `mneme cache prune`           delete snapshot DBs older than N days
//!   `mneme cache gc`              VACUUM + WAL-truncate every shard DB
//!   `mneme cache drop <project>`  delete a project's entire cache
//!
//! All write paths support `--dry-run` (read-only inspection).
//! `drop` requires a `yes` confirmation unless `--yes` is passed.

use clap::{Args, Subcommand};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::error::{CliError, CliResult};
use common::{ids::ProjectId, paths::PathManager};

/// CLI args for `mneme cache`.
#[derive(Debug, Args)]
pub struct CacheArgs {
    #[command(subcommand)]
    pub op: CacheOp,
}

/// Subcommands for `mneme cache`.
#[derive(Debug, Subcommand)]
pub enum CacheOp {
    /// Show disk-usage breakdown across mneme cache directories.
    Du {
        /// Output as JSON instead of human-readable table.
        #[arg(long)]
        json: bool,
    },

    /// Delete snapshot DBs older than the threshold age, or with
    /// `--baks`, prune `<config>.mneme-*.bak` install backups by count.
    Prune {
        /// Age threshold (e.g. `30d`, `7d`, `24h`, `1w`). Defaults to 30 days.
        #[arg(long, default_value = "30d")]
        older_than: String,
        /// Limit to a single project (defaults to all).
        #[arg(long)]
        project: Option<PathBuf>,
        /// Print what would be deleted but don't delete.
        #[arg(long)]
        dry_run: bool,
        /// Idempotent-1: instead of pruning project snapshot DBs, prune
        /// the `<config>.mneme-YYYYMMDD-HHMMSS.bak` install backups under
        /// the platform config dirs (`~/.claude/`, `~/.cursor/`, etc.).
        /// Each install creates a NEW timestamped snapshot of every
        /// modified config file; without retention they accumulate.
        /// With `--baks`, keep only the `--keep` most-recent snapshots
        /// per source path. `--older-than` is ignored when `--baks` is
        /// set (retention here is by count, not age).
        #[arg(long)]
        baks: bool,
        /// Idempotent-1: how many `.mneme-*.bak` snapshots to retain per
        /// source path when `--baks` is set. Defaults to 5.
        #[arg(long, default_value_t = 5)]
        keep: usize,
    },

    /// Run VACUUM + wal_checkpoint(TRUNCATE) on every shard DB.
    Gc {
        /// Limit to a single project (defaults to all).
        #[arg(long)]
        project: Option<PathBuf>,
        /// Print what would run but don't write.
        #[arg(long)]
        dry_run: bool,
    },

    /// Drop a single project's entire cache (projects/<id> + snapshots/<id>).
    /// Destructive — prompts for `yes` unless `--yes` is passed.
    Drop {
        /// Project root path (required).
        project: PathBuf,
        /// Skip the interactive confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

/// Entry point used by `main.rs`.
pub async fn run(args: CacheArgs) -> CliResult<()> {
    match args.op {
        CacheOp::Du { json } => run_du(json),
        CacheOp::Prune {
            older_than,
            project,
            dry_run,
            baks,
            keep,
        } => {
            if baks {
                run_prune_baks(keep, dry_run)
            } else {
                run_prune(&older_than, project.as_deref(), dry_run)
            }
        }
        CacheOp::Gc { project, dry_run } => run_gc(project.as_deref(), dry_run),
        CacheOp::Drop { project, yes } => run_drop(&project, yes),
    }
}

// ---------------------------------------------------------------------------
// du — disk-usage report
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct DuReport {
    bin_bytes: u64,
    cache_bytes: u64,
    models_bytes: u64,
    projects_bytes: u64,
    snapshots_bytes: u64,
    install_receipts_bytes: u64,
    meta_bytes: u64,
    total_bytes: u64,
    per_project: Vec<ProjectDu>,
}

#[derive(Debug, Serialize)]
struct ProjectDu {
    id: String,
    shards_bytes: u64,
    snapshots_bytes: u64,
}

fn run_du(json: bool) -> CliResult<()> {
    let paths = PathManager::default_root();
    let root = paths.root();

    let bin = dir_size(&root.join("bin"));
    let cache = dir_size(&root.join("cache"));
    let models = dir_size(&root.join("models"));
    let projects_dir = root.join("projects");
    let snapshots_dir = root.join("snapshots");
    let projects = dir_size(&projects_dir);
    let snapshots = dir_size(&snapshots_dir);
    let receipts = dir_size(&root.join("install-receipts"));
    let meta = file_size(&root.join("meta.db"))
        + file_size(&root.join("meta.db-wal"))
        + file_size(&root.join("meta.db-shm"));
    let total = bin + cache + models + projects + snapshots + receipts + meta;

    let mut per_project: Vec<ProjectDu> = Vec::new();
    if let Ok(entries) = fs::read_dir(&projects_dir) {
        for e in entries.flatten() {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let id = e.file_name().to_string_lossy().to_string();
            let shards = dir_size(&e.path());
            let snaps = dir_size(&snapshots_dir.join(&id));
            per_project.push(ProjectDu {
                id,
                shards_bytes: shards,
                snapshots_bytes: snaps,
            });
        }
    }
    per_project.sort_by(|a, b| {
        (b.shards_bytes + b.snapshots_bytes).cmp(&(a.shards_bytes + a.snapshots_bytes))
    });

    let report = DuReport {
        bin_bytes: bin,
        cache_bytes: cache,
        models_bytes: models,
        projects_bytes: projects,
        snapshots_bytes: snapshots,
        install_receipts_bytes: receipts,
        meta_bytes: meta,
        total_bytes: total,
        per_project,
    };

    if json {
        let s = serde_json::to_string_pretty(&report)
            .map_err(|e| CliError::Other(format!("serde: {e}")))?;
        println!("{s}");
    } else {
        println!(
            "mneme cache du — {} total under {}",
            human(total),
            root.display()
        );
        println!();
        println!("  {:>10}  bin/             (compiled binaries)", human(bin));
        println!(
            "  {:>10}  cache/           (docs / embed / multimodal cache)",
            human(cache)
        );
        println!("  {:>10}  models/          (LLM weights)", human(models));
        println!(
            "  {:>10}  projects/        ({} project shard dir(s))",
            human(projects),
            report.per_project.len()
        );
        println!(
            "  {:>10}  snapshots/       (point-in-time DB copies)",
            human(snapshots)
        );
        println!("  {:>10}  install-receipts/", human(receipts));
        println!("  {:>10}  meta.db          (global metadata)", human(meta));

        if !report.per_project.is_empty() {
            println!();
            println!("  per project (sorted by total):");
            for p in &report.per_project {
                let total_p = p.shards_bytes + p.snapshots_bytes;
                let short = &p.id[..16.min(p.id.len())];
                println!(
                    "    {}...  {:>10}  (shards: {} + snapshots: {})",
                    short,
                    human(total_p),
                    human(p.shards_bytes),
                    human(p.snapshots_bytes)
                );
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// prune — delete snapshot DBs older than threshold
// ---------------------------------------------------------------------------

fn run_prune(older_than: &str, project_path: Option<&Path>, dry_run: bool) -> CliResult<()> {
    let cutoff_age = parse_age(older_than)?;
    let now = SystemTime::now();
    let cutoff_time = now
        .checked_sub(cutoff_age)
        .ok_or_else(|| CliError::Other(format!("cutoff age {older_than} too large")))?;

    let paths = PathManager::default_root();
    let snapshots_dir = paths.root().join("snapshots");

    let project_dirs: Vec<PathBuf> = if let Some(p) = project_path {
        let id = ProjectId::from_path(p)
            .map_err(|e| CliError::Other(format!("project hash for {}: {e}", p.display())))?;
        vec![snapshots_dir.join(id.to_string())]
    } else {
        match fs::read_dir(&snapshots_dir) {
            Ok(entries) => entries
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.path())
                .collect(),
            Err(_) => Vec::new(),
        }
    };

    let mut total_freed: u64 = 0;
    let mut total_removed: usize = 0;
    let prefix = if dry_run {
        "(dry-run) would delete"
    } else {
        "deleted"
    };

    for proj_dir in &project_dirs {
        if !proj_dir.exists() {
            continue;
        }
        let entries = match fs::read_dir(proj_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.extension().map(|e| e == "db").unwrap_or(false) {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let modified = match meta.modified() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if modified >= cutoff_time {
                continue;
            }
            let size = meta.len();
            println!("  {} {} ({})", prefix, path.display(), human(size));
            if !dry_run {
                if let Err(e) = fs::remove_file(&path) {
                    eprintln!("    failed: {e}");
                    continue;
                }
            }
            total_freed = total_freed.saturating_add(size);
            total_removed += 1;
        }
    }

    println!();
    let action = if dry_run { "would free" } else { "freed" };
    println!(
        "{action} {} across {total_removed} snapshot file(s) older than {older_than}",
        human(total_freed)
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// prune --baks — Idempotent-1: trim accumulated install backups
// ---------------------------------------------------------------------------
//
// Every `mneme install` call writes a fresh `<config>.mneme-YYYYMMDD-HHMMSS.bak`
// snapshot of every modified config (see `platforms/mod.rs::backup_then_write`).
// Without retention these accumulate one-per-install per source path. The
// install pipeline now auto-prunes after each write (DEFAULT_BAK_RETAIN), but
// this subcommand is the user-facing handle for explicit cleanup runs and
// for trimming after upgrades from older mneme versions that didn't prune.
//
// Strategy: walk the well-known platform config dirs, find every
// `*.mneme-YYYYMMDD-HHMMSS.bak` file, group by the implicit source-path
// (filename minus the `.mneme-<stamp>.bak` suffix), then keep the
// `keep` most-recent per group.

fn run_prune_baks(keep: usize, dry_run: bool) -> CliResult<()> {
    // Well-known dirs where mneme writes platform config snapshots. We
    // walk each NON-recursively (the snapshot files live next to their
    // source config, never in subdirs).
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => {
            return Err(CliError::Other(
                "cannot locate home dir for platform config scan".into(),
            ))
        }
    };
    let scan_dirs: Vec<PathBuf> = vec![
        home.join(".claude"),
        home.join(".cursor"),
        home.join(".codeium"),
        home.join(".config").join("Code").join("User"),
        home.join(".config").join("zed"),
        home.join(".config").join("kiro"),
        home.join(".windsurf"),
        home.join(".aider"),
        home.join(".gemini"),
        home.join(".opencode"),
        home.join(".qoder"),
        // Also scan the mneme home itself in case any future surface
        // backs up files into ~/.mneme/.
        home.join(".mneme"),
    ];

    let mut total_groups: usize = 0;
    let mut total_removed: usize = 0;
    let mut total_freed: u64 = 0;

    for dir in &scan_dirs {
        if !dir.exists() {
            continue;
        }
        // Group `*.mneme-YYYYMMDD-HHMMSS.bak` files by their source-path
        // stem (the part before `.mneme-`). We discover those stems by
        // scanning the dir once.
        let mut groups: std::collections::BTreeMap<String, Vec<PathBuf>> =
            std::collections::BTreeMap::new();
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = match path.file_name().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            // Match `<stem>.mneme-YYYYMMDD-HHMMSS.bak`.
            if !name.ends_with(".bak") {
                continue;
            }
            // Find `.mneme-` boundary: stem before, stamp+`.bak` after.
            let mneme_idx = match name.find(".mneme-") {
                Some(i) => i,
                None => continue,
            };
            let stem = name[..mneme_idx].to_string();
            groups.entry(stem).or_default().push(path);
        }

        for (_stem, mut snapshots) in groups {
            total_groups += 1;
            // Descending filename sort: newest stamp first.
            snapshots.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
            if snapshots.len() <= keep {
                continue;
            }
            let to_delete: Vec<PathBuf> = snapshots.into_iter().skip(keep).collect();
            for p in &to_delete {
                let size = p.metadata().map(|m| m.len()).unwrap_or(0);
                let prefix = if dry_run {
                    "(dry-run) would delete"
                } else {
                    "deleted"
                };
                println!("  {prefix} {} ({})", p.display(), human(size));
                if !dry_run {
                    if let Err(e) = fs::remove_file(p) {
                        eprintln!("    failed: {e}");
                        continue;
                    }
                }
                total_removed += 1;
                total_freed = total_freed.saturating_add(size);
            }
        }
    }

    println!();
    let action = if dry_run { "would free" } else { "freed" };
    println!(
        "{action} {} across {total_removed} install snapshot(s) (groups scanned: {total_groups}, keep={keep})",
        human(total_freed)
    );
    Ok(())
}

/// Parse ages like `30d`, `7d`, `24h`, `1w`. Accepts s/m/h/d/w units.
fn parse_age(s: &str) -> CliResult<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(CliError::Other("empty age string".into()));
    }
    let (num_str, unit) = s.split_at(s.len() - 1);
    let n: u64 = num_str
        .parse()
        .map_err(|_| CliError::Other(format!("invalid age {s}: expected like 30d, 7d, 24h, 1w")))?;
    let secs = match unit {
        "s" => Some(n),
        "m" => n.checked_mul(60),
        "h" => n.checked_mul(3600),
        "d" => n.checked_mul(86400),
        "w" => n.checked_mul(604800),
        other => {
            return Err(CliError::Other(format!(
                "unknown age unit '{other}' — use s/m/h/d/w"
            )))
        }
    }
    .ok_or_else(|| CliError::Other(format!("age {s} overflows")))?;
    Ok(Duration::from_secs(secs))
}

// ---------------------------------------------------------------------------
// gc — VACUUM + wal_checkpoint(TRUNCATE) per shard
// ---------------------------------------------------------------------------

fn run_gc(project_path: Option<&Path>, dry_run: bool) -> CliResult<()> {
    let paths = PathManager::default_root();
    let projects_dir = paths.root().join("projects");

    let project_ids: Vec<String> = if let Some(p) = project_path {
        let id = ProjectId::from_path(p)
            .map_err(|e| CliError::Other(format!("project hash for {}: {e}", p.display())))?;
        vec![id.to_string()]
    } else {
        match fs::read_dir(&projects_dir) {
            Ok(entries) => entries
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect(),
            Err(_) => Vec::new(),
        }
    };

    let mut dbs_processed: usize = 0;
    let mut bytes_before: u64 = 0;
    let mut bytes_after: u64 = 0;

    for id in &project_ids {
        let proj_dir = projects_dir.join(id);
        if !proj_dir.exists() {
            continue;
        }
        let entries = match fs::read_dir(&proj_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.extension().map(|e| e == "db").unwrap_or(false) {
                continue;
            }
            let before = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if dry_run {
                println!(
                    "  (dry-run) would VACUUM {} (current size {})",
                    path.display(),
                    human(before)
                );
                bytes_before = bytes_before.saturating_add(before);
                dbs_processed += 1;
                continue;
            }
            let conn = match Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("  skip {}: open failed: {e}", path.display());
                    continue;
                }
            };
            // wal_checkpoint(TRUNCATE) flushes WAL into main DB and shrinks the
            // -wal file. Best-effort — failure here doesn't block VACUUM.
            if let Err(e) = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE") {
                eprintln!("  wal_checkpoint warn for {}: {e}", path.display());
            }
            if let Err(e) = conn.execute_batch("VACUUM;") {
                eprintln!("  VACUUM failed for {}: {e}", path.display());
                continue;
            }
            drop(conn);
            let after = path.metadata().map(|m| m.len()).unwrap_or(0);
            let saved_pct = if before > 0 {
                (before.saturating_sub(after)) as i64 * 100 / before as i64
            } else {
                0
            };
            println!(
                "  vacuumed {}  {} -> {} ({}% reduction)",
                path.display(),
                human(before),
                human(after),
                saved_pct
            );
            bytes_before = bytes_before.saturating_add(before);
            bytes_after = bytes_after.saturating_add(after);
            dbs_processed += 1;
        }
    }

    println!();
    let action = if dry_run {
        "would process"
    } else {
        "processed"
    };
    println!(
        "{action} {dbs_processed} db file(s) across {} project(s)",
        project_ids.len()
    );
    if !dry_run && bytes_before > 0 {
        let saved = bytes_before.saturating_sub(bytes_after);
        let pct = saved * 100 / bytes_before;
        println!(
            "freed: {} ({}% of {})",
            human(saved),
            pct,
            human(bytes_before)
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// drop — delete a project's entire cache
// ---------------------------------------------------------------------------

fn run_drop(project_path: &Path, yes: bool) -> CliResult<()> {
    let id = ProjectId::from_path(project_path).map_err(|e| {
        CliError::Other(format!("project hash for {}: {e}", project_path.display()))
    })?;
    let paths = PathManager::default_root();
    let proj_dir = paths.root().join("projects").join(id.to_string());
    let snap_dir = paths.root().join("snapshots").join(id.to_string());

    let proj_size = dir_size(&proj_dir);
    let snap_size = dir_size(&snap_dir);
    let total = proj_size.saturating_add(snap_size);

    if total == 0 && !proj_dir.exists() && !snap_dir.exists() {
        println!(
            "nothing to drop — no cache for project {} (id {})",
            project_path.display(),
            id
        );
        return Ok(());
    }

    println!("about to drop:");
    if proj_dir.exists() {
        println!("  {}  ({})", proj_dir.display(), human(proj_size));
    }
    if snap_dir.exists() {
        println!("  {}  ({})", snap_dir.display(), human(snap_size));
    }
    println!("  total: {}", human(total));

    if !yes {
        use std::io::Write;
        print!("type 'yes' to confirm: ");
        std::io::stdout().flush().ok();
        let mut s = String::new();
        std::io::stdin()
            .read_line(&mut s)
            .map_err(|e| CliError::Other(format!("read stdin: {e}")))?;
        if s.trim() != "yes" {
            println!("aborted");
            return Ok(());
        }
    }

    if proj_dir.exists() {
        fs::remove_dir_all(&proj_dir).map_err(|e| {
            CliError::Other(format!("failed to remove {}: {e}", proj_dir.display()))
        })?;
    }
    if snap_dir.exists() {
        fs::remove_dir_all(&snap_dir).map_err(|e| {
            CliError::Other(format!("failed to remove {}: {e}", snap_dir.display()))
        })?;
    }
    println!("dropped {} for project {}", human(total), id);
    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn dir_size(path: &Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    fn walk(path: &Path) -> u64 {
        let mut total: u64 = 0;
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata() {
                    if meta.is_file() {
                        total = total.saturating_add(meta.len());
                    } else if meta.is_dir() {
                        total = total.saturating_add(walk(&entry.path()));
                    }
                }
            }
        }
        total
    }
    walk(path)
}

fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn human(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct Harness {
        #[command(flatten)]
        args: CacheArgs,
    }

    #[test]
    fn parse_age_30d() {
        assert_eq!(parse_age("30d").unwrap(), Duration::from_secs(30 * 86400));
    }

    #[test]
    fn parse_age_24h() {
        assert_eq!(parse_age("24h").unwrap(), Duration::from_secs(24 * 3600));
    }

    #[test]
    fn parse_age_1w() {
        assert_eq!(parse_age("1w").unwrap(), Duration::from_secs(604800));
    }

    #[test]
    fn parse_age_invalid_unit() {
        assert!(parse_age("30y").is_err());
    }

    #[test]
    fn parse_age_empty() {
        assert!(parse_age("").is_err());
    }

    #[test]
    fn parse_age_bad_number() {
        assert!(parse_age("xd").is_err());
    }

    #[test]
    fn human_units() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(512), "512 B");
        assert_eq!(human(2048), "2.00 KB");
        assert_eq!(human(2 * 1024 * 1024), "2.00 MB");
        assert_eq!(human(3 * 1024 * 1024 * 1024), "3.00 GB");
    }

    #[test]
    fn cache_du_subcmd_parses() {
        let h = Harness::try_parse_from(["x", "du"]).unwrap();
        assert!(matches!(h.args.op, CacheOp::Du { json: false }));
    }

    #[test]
    fn cache_du_json_subcmd_parses() {
        let h = Harness::try_parse_from(["x", "du", "--json"]).unwrap();
        assert!(matches!(h.args.op, CacheOp::Du { json: true }));
    }

    #[test]
    fn cache_prune_default_age() {
        let h = Harness::try_parse_from(["x", "prune"]).unwrap();
        match h.args.op {
            CacheOp::Prune {
                older_than,
                dry_run,
                ..
            } => {
                assert_eq!(older_than, "30d");
                assert!(!dry_run);
            }
            _ => panic!("expected Prune"),
        }
    }

    #[test]
    fn cache_prune_dry_run() {
        let h = Harness::try_parse_from(["x", "prune", "--older-than", "7d", "--dry-run"]).unwrap();
        match h.args.op {
            CacheOp::Prune {
                older_than,
                dry_run,
                ..
            } => {
                assert_eq!(older_than, "7d");
                assert!(dry_run);
            }
            _ => panic!("expected Prune"),
        }
    }

    #[test]
    fn cache_gc_subcmd_parses() {
        let h = Harness::try_parse_from(["x", "gc"]).unwrap();
        assert!(matches!(
            h.args.op,
            CacheOp::Gc {
                project: None,
                dry_run: false
            }
        ));
    }

    #[test]
    fn cache_drop_requires_project() {
        let h = Harness::try_parse_from(["x", "drop"]);
        assert!(h.is_err());
    }

    #[test]
    fn cache_drop_with_project() {
        let h = Harness::try_parse_from(["x", "drop", "/some/path", "--yes"]).unwrap();
        match h.args.op {
            CacheOp::Drop { project, yes } => {
                assert_eq!(project, PathBuf::from("/some/path"));
                assert!(yes);
            }
            _ => panic!("expected Drop"),
        }
    }

    #[test]
    fn dir_size_empty_returns_zero() {
        let tmp = std::env::temp_dir().join("mneme_test_cache_du_empty");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        assert_eq!(dir_size(&tmp), 0);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn dir_size_counts_nested_files() {
        let tmp = std::env::temp_dir().join("mneme_test_cache_du_nested");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("a/b")).unwrap();
        fs::write(tmp.join("a/file1.txt"), b"hello").unwrap(); // 5 bytes
        fs::write(tmp.join("a/b/file2.txt"), vec![0u8; 100]).unwrap(); // 100 bytes
        assert_eq!(dir_size(&tmp), 105);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn dir_size_nonexistent_returns_zero() {
        let p = PathBuf::from("/nonexistent_path_for_mneme_test");
        assert_eq!(dir_size(&p), 0);
    }
}
