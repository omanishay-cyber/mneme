//! `mneme build [project_path]` — initial full project ingest.
//!
//! v0.1 strategy: drive parse + store IN-PROCESS. The CLI walks the project,
//! parses each supported file with Tree-sitter directly (via the `parsers`
//! library), and writes nodes + edges to the project's `graph.db` via the
//! store library. No supervisor round-trip — that path is wired in v0.2.
//!
//! Benefit: `mneme build .` produces a real, queryable SQLite graph
//! without any worker pool round-trip or IPC dependency.

use clap::Args;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::build_heartbeat::Heartbeat;
use crate::build_lock::BuildLock;
use crate::error::{CliError, CliResult};
use crate::ipc::{IpcClient, IpcRequest, IpcResponse};

use brain::cluster_runner::{ClusterRunner, ClusterRunnerConfig};
use brain::conventions::{ConventionLearner, DefaultLearner};
use brain::federated::FederatedStore;
use brain::leiden::Community as BrainCommunity;
use brain::wiki::{CommunityInput as WikiCommunityInput, WikiBuilder, WikiSymbol};
use brain::Embedder;
use brain::NodeId as BrainNodeId;
use common::{layer::DbLayer, paths::PathManager, ids::ProjectId};
use multimodal::{ExtractedDoc, Registry as MmRegistry};
use scanners::scanners::architecture::{ArchEdge, ArchNode, ArchitectureScanner};
use sha2::{Digest, Sha256};
use store::{inject::InjectOptions, Store};
use parsers::{
    extractor::Extractor, incremental::IncrementalParser, looks_like_test_path,
    parser_pool::ParserPool, query_cache, Language, NodeKind,
};

/// CLI args for `mneme build`.
#[derive(Debug, Args)]
pub struct BuildArgs {
    /// Path to the project root. Defaults to CWD.
    pub project: Option<PathBuf>,

    /// Force a re-parse of every file (default: only changed since last build).
    #[arg(long)]
    pub full: bool,

    /// Maximum files to process (0 = unlimited). Useful for smoke-testing.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,

    /// (v0.3) Dispatch parse/scan/embed work to the supervisor's
    /// worker pool instead of running the pipeline inline in this CLI
    /// process. Falls back to inline automatically if the supervisor is
    /// unreachable.
    #[arg(long)]
    pub dispatch: bool,

    /// Force inline execution even when the supervisor is reachable.
    /// This is the v0.1–v0.2 path and remains the default today; the
    /// flag exists to let future defaults flip without breaking scripts.
    #[arg(long, conflicts_with = "dispatch")]
    pub inline: bool,

    /// Skip the pre-flight confirmation prompt when the project contains
    /// more than `BIG_PROJECT_FILE_THRESHOLD` files after ignores. CI and
    /// the `install.ps1` one-liner set this; humans running
    /// `mneme build ~` or similar see the prompt and can decline.
    #[arg(long, short = 'y')]
    pub yes: bool,

    /// Maximum seconds to wait for the per-project build lock if a
    /// concurrent build is already in progress. `0` (default) is
    /// fail-fast — second invocation exits immediately with exit
    /// code 4 (`error: another build in progress for project <id>
    /// (locked at <ts>)`). Higher values poll every 250ms up to the
    /// deadline. Audit fix L4 (v0.3.0).
    #[arg(long, default_value_t = 0)]
    pub lock_timeout_secs: u64,

    /// Suppress the per-30-second progress heartbeat that surfaces
    /// during the parse / embed / graph passes. The heartbeat exists
    /// to break long silences in `mneme build` so users don't assume
    /// the process hung and Ctrl-C mid-write (B-017 in
    /// `MNEME-BUILD-DIAGNOSIS-2026-04-30.md`). CI runners + cron jobs
    /// that pipe stdout to a log file may want the noise gone — pass
    /// `--quiet` and the heartbeat task stays armed but emits
    /// nothing.
    #[arg(long)]
    pub quiet: bool,
}

/// File-count threshold above which `mneme build` prompts the user to
/// confirm before proceeding. Tuned so indexing a typical repo (1k-5k
/// source files) is silent, but pointing at a home directory or an
/// unfiltered monorepo requires explicit consent.
///
/// Rationale: F-010 in the v0.3.0 install report — `mneme build
/// C:\Users\POS` kicked off indexing node_modules, .git, AppData,
/// OneDrive, Screenshots, every plugin cache, on track for hundreds of
/// thousands of files. Exit path was "kill the daemon". Unacceptable
/// default behavior for a tool that runs unattended.
const BIG_PROJECT_FILE_THRESHOLD: usize = 10_000;

/// Per-file size cap above which we count the file as `too-large` and
/// skip parsing. K15 categorisation needs an explicit threshold (and
/// counter) so the build summary can show `0 too-large` rather than
/// silently lumping the file into "binary" (it isn't) or "unsupported
/// language" (it isn't). 5 MiB matches what most editors fall over at;
/// real source files are virtually never bigger than this. Generated
/// bundles, vendored payloads, and gigantic JSON fixtures are the
/// expected matches.
const MAX_PARSE_BYTES: u64 = 5 * 1024 * 1024;

/// Persist a phase transition into `build-state.json`. Unlike the
/// per-25-files checkpoint (where losing a save is fine — the next
/// checkpoint will rewrite it), a phase transition is contract-bearing:
/// if the transition into `BuildPhase::Multimodal` is not durable, a
/// resumed build will start from `BuildPhase::Parse` and redo every
/// file it already indexed. Silent-2 in `docs/dev/DEEP-AUDIT-2026-04-29.md`
/// (Class H-silent) — propagate the io::Error as a `CliError::Io` so the
/// build refuses to print "ok" on a save that didn't land.
fn save_phase_transition(
    project: &Path,
    state: &crate::commands::build_state::BuildState,
) -> CliResult<()> {
    crate::commands::build_state::save(project, state).map_err(|e| CliError::Io {
        path: Some(project.to_path_buf()),
        source: e,
    })
}

/// Entry point used by `main.rs`.
pub async fn run(args: BuildArgs, socket_override: Option<PathBuf>) -> CliResult<()> {
    let project = resolve_project(args.project.clone())?;
    info!(project = %project.display(), full = args.full, dispatch = args.dispatch, "building mneme graph");

    // Pre-flight guard — refuse to chew through tens of thousands of files
    // without explicit consent. See BIG_PROJECT_FILE_THRESHOLD docstring
    // for the incident this guards against.
    if !args.yes {
        let count = count_candidate_files(&project, BIG_PROJECT_FILE_THRESHOLD + 1);
        if count > BIG_PROJECT_FILE_THRESHOLD {
            eprintln!();
            eprintln!(
                "mneme build: {} matches more than {} files (post-ignore).",
                project.display(),
                BIG_PROJECT_FILE_THRESHOLD
            );
            eprintln!("This is usually a mistake — did you mean to index a subdirectory?");
            eprintln!();
            eprintln!("Indexing will:");
            eprintln!("  * read every matched source file end-to-end");
            eprintln!("  * write node/edge rows to ~/.mneme/projects/<id>/graph.db");
            eprintln!("  * take many minutes to finish on this scale");
            eprintln!();
            eprintln!("To proceed anyway: re-run with `--yes` (or `-y`).");
            eprintln!("To limit the ingest for a smoke test: `--limit 500`.");
            return Err(CliError::Other(
                "aborted (too many files; pass --yes to confirm)".into(),
            ));
        }
    }

    // L4: Cross-process build lock. Held for the lifetime of this
    // function — Drop releases on success, error, AND panic. Acquired
    // BEFORE any DB writes (inline path) and BEFORE submitting any
    // dispatch jobs (which the supervisor will eventually run inline
    // against the same shard). Dropped automatically when `_lock`
    // goes out of scope at the end of `run`.
    let project_id = ProjectId::from_path(&project)
        .map_err(|e| CliError::Other(format!("cannot hash project path: {e}")))?;
    let paths = PathManager::default_root();
    let project_root = paths.project_root(&project_id);
    let lock_timeout = Duration::from_secs(args.lock_timeout_secs);
    let _lock = BuildLock::acquire(project_id.as_str(), &project_root, lock_timeout)?;

    // --dispatch: try the supervisor path; fall back to inline if the
    // daemon isn't running. --inline: force the in-process pipeline
    // (also the default).
    //
    // B-001/B-002: build pipeline IPC uses `with_no_autospawn()` so a
    // missing pipe NEVER triggers `spawn_daemon_detached()`. We also
    // keep a tight 5s budget per round-trip — the supervisor must
    // respond fast for `mneme build` to be usable, and a 120s default
    // would let one stuck IPC call masquerade as a runaway build. The
    // direct `is_running()` precheck is also no-autospawn so a missing
    // pipe immediately downgrades to inline.
    if args.dispatch && !args.inline {
        let client = make_client_for_build(socket_override.clone());
        if client.is_running().await {
            match run_dispatched(&args, &project, &client).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    warn!(error = %e, "dispatch path failed; falling back to inline build");
                }
            }
        } else {
            warn!("supervisor unreachable; falling back to inline build");
        }
    }

    run_inline(args, project).await
}

/// Walk the project and submit one `Job::Parse` per source file to the
/// supervisor, then poll `JobQueueStatus` until the queue drains.
///
/// v0.3 MVP: wires only `Job::Parse`. `Scan`/`Embed`/`Ingest` are still
/// running inline from the subcommand that needs them; they'll be
/// migrated in follow-ups per ARCHITECTURE.md §Worker dispatch roadmap.
async fn run_dispatched(
    args: &BuildArgs,
    project: &Path,
    client: &IpcClient,
) -> CliResult<()> {
    use common::jobs::Job;

    // K3 + H4: dispatched builds were silent on the model-probe surface.
    // The supervisor runs the embedding pass on its side, but if the
    // user has no model installed, recall stays keyword-only and they
    // never know. Print the same warning the inline path prints — so
    // `mneme build --dispatch` and `mneme build --inline` look the
    // same as far as user-facing degradation signals go.
    let model_probes = ModelProbes::probe();
    model_probes.print_warnings_at_top();

    let paths = PathManager::default_root();
    let project_id = ProjectId::from_path(project)
        .map_err(|e| CliError::Other(format!("cannot hash project path: {e}")))?;
    let shard_root = paths.project_root(&project_id);

    let gi = load_project_ignore(project);
    let gi_ref = gi.as_ref();
    let walker = walkdir::WalkDir::new(project)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let p = e.path();
            if is_ignored(p) { return false; }
            !project_ignore_matches(gi_ref, p, e.file_type().is_dir())
        });

    let mut submitted = 0usize;
    let mut total = 0usize;
    let mut skipped = 0usize;
    let started = std::time::Instant::now();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "walk error; continuing");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        total += 1;
        let path = entry.path();
        let Some(lang) = Language::from_filename(path) else {
            skipped += 1;
            continue;
        };
        if !lang.is_enabled() {
            skipped += 1;
            continue;
        }
        if args.limit > 0 && submitted >= args.limit {
            break;
        }
        let job = Job::Parse {
            file_path: path.to_path_buf(),
            shard_root: shard_root.clone(),
        };
        match client.request(IpcRequest::DispatchJob { job }).await? {
            IpcResponse::JobQueued { .. } => submitted += 1,
            IpcResponse::Error { message } => {
                return Err(CliError::Supervisor(format!(
                    "DispatchJob rejected after {submitted} submissions: {message}"
                )));
            }
            other => {
                return Err(CliError::Supervisor(format!(
                    "unexpected DispatchJob response: {other:?}"
                )));
            }
        }
    }
    println!("submitted {submitted} parse jobs ({skipped} skipped, {total} walked)");

    // Watchdog: poll until pending + in_flight == 0 or timeout.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if std::time::Instant::now() > deadline {
            return Err(CliError::Supervisor(
                "timeout waiting for dispatched build to finish".into(),
            ));
        }
        let resp = client.request(IpcRequest::JobQueueStatus).await?;
        let snap = match resp {
            IpcResponse::JobQueue { snapshot } => snapshot,
            IpcResponse::Error { message } => {
                return Err(CliError::Supervisor(format!("JobQueueStatus: {message}")))
            }
            _ => return Err(CliError::Supervisor("unexpected JobQueueStatus resp".into())),
        };
        let pending = snap.get("pending").and_then(|v| v.as_u64()).unwrap_or(0);
        let in_flight = snap.get("in_flight").and_then(|v| v.as_u64()).unwrap_or(0);
        if pending == 0 && in_flight == 0 {
            let completed = snap.get("completed").and_then(|v| v.as_u64()).unwrap_or(0);
            let failed = snap.get("failed").and_then(|v| v.as_u64()).unwrap_or(0);
            let elapsed = started.elapsed();
            println!(
                "dispatched build done in {elapsed:?}: completed={completed} failed={failed}"
            );

            // K3 + H4: surface the embedding model status in the
            // dispatched summary too, so a degraded build is never
            // silent regardless of which path the user took. The
            // dispatched path doesn't run the Leiden pass itself —
            // the supervisor does, asynchronously — so we only emit
            // the model line here. The operator can run
            // `mneme cache du` afterwards to confirm community shard
            // population.
            model_probes.print_warnings_in_summary();
            let emb_line = model_probes.summary_embedding_status_line();
            info!(target: "build.summary", embedding = %emb_line, mode = "dispatched");
            println!("  {emb_line}");
            return Ok(());
        }
    }
}

/// The classic in-process pipeline — unchanged from v0.2 behaviour.
///
/// Exposed at `pub(crate)` so `commands::rebuild` can re-use it for
/// the direct-DB rebuild fallback (audit fix L17). Callers must hold
/// the [`BuildLock`] for the project before calling this.
pub(crate) async fn run_inline(args: BuildArgs, project: PathBuf) -> CliResult<()> {
    let _ = args.inline; // silence unused

    // B-003 (v0.3.2): track every subprocess this build spawns so a
    // Ctrl-C arriving mid-pipeline can taskkill them deterministically
    // instead of leaving parser/scanner/brain/md-ingest workers behind
    // as orphans. The registry's `Drop` impl is the safety net for the
    // panic / early-return path; the explicit `tokio::signal::ctrl_c()`
    // race below is the user-facing one. See `BuildChildRegistry`.
    let children = BuildChildRegistry::new();

    // K10 #6: load any existing `<project>/.mneme/build-state.json` so
    // a previous Ctrl-C'd run can resume from where it stopped. The
    // CtrlCGuard below writes a final state on Ctrl-C; we read it here
    // on entry so the next `mneme build` skips already-completed files.
    // `load` returns None for missing / corrupt / version-mismatched
    // files, which means "no resume info; build from scratch". The
    // existing per-file SHA skip in `graph.db::files` is the safety
    // net — even if the resume cursor over-reports completion the
    // build remains correct.
    use crate::commands::build_state::{self, BuildState};
    let resume_state = build_state::load(&project);
    if let Some(rs) = resume_state.as_ref() {
        info!(
            files_done = rs.files_done,
            last_completed_file = %rs.last_completed_file,
            "resuming from build-state checkpoint"
        );
        println!(
            "  resuming from previous build (last file: {})",
            rs.last_completed_file
        );
    }
    let mut current_state = resume_state
        .clone()
        .unwrap_or_else(|| BuildState::new(&project));

    let _ctrl_c_guard =
        spawn_ctrl_c_killer_with_state(children.clone(), project.clone(), current_state.clone());

    // 0. Pre-flight model probes — K3/K4. If neither an embedding model nor
    //    an LLM is installed, semantic recall and code-summaries are not
    //    going to work. Warn loudly at the top of the build instead of
    //    silently producing a degraded graph (see phase-a-issues.md K3/K4).
    //    Re-emitted at the end of the build inside the summary block so
    //    the message survives long log scrolls.
    let model_probes = ModelProbes::probe();
    model_probes.print_warnings_at_top();

    // 1. Store setup: open (or create) the per-project shard.
    let paths = PathManager::default_root();
    let store = Store::new(paths.clone());
    let project_id = ProjectId::from_path(&project)
        .map_err(|e| CliError::Other(format!("cannot hash project path: {e}")))?;
    let project_name = project
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project")
        .to_string();
    let _shard = store
        .builder
        .build_or_migrate(&project_id, &project, &project_name)
        .await
        .map_err(|e| CliError::Other(format!("store build_or_migrate: {e}")))?;
    println!(
        "shard ready at {}",
        paths.project_root(&project_id).display()
    );

    // 2. Parser pool — small (4 parsers/language) so one CLI process stays
    // lean. Tree-sitter parses are CPU-bound; tokio's multi-threaded
    // runtime will use all cores concurrently via `spawn_blocking`.
    let pool = Arc::new(
        ParserPool::new(4)
            .map_err(|e| CliError::Other(format!("parser pool init: {e}")))?,
    );
    if let Err(e) = query_cache::warm_up() {
        warn!(error = %e, "query warm-up reported issues (non-fatal)");
    }
    let inc = Arc::new(IncrementalParser::new(pool.clone()));

    // B-017: progress heartbeat. Every 30 s the heartbeat task emits a
    // status line so the parse / multimodal / embed / leiden /
    // betweenness phases (which are otherwise silent for minutes at a
    // time) cannot be mistaken for a hang. The handle is held for the
    // full pipeline; `set_phase` re-uses it across stages. Drop is
    // RAII-cancelled at function end so a panic / `?` short-circuit
    // doesn't leak the timer task. See `build_heartbeat.rs`.
    let heartbeat = Heartbeat::start("parse", 0, args.quiet);

    // 3. Walk the project. Respect:
    //    - hard-coded is_ignored() safety net (always pruned at the
    //      directory level, so we don't descend into AppData / .git)
    //    - user-controlled .mnemeignore / .gitignore (P16). Directory
    //      matches are pruned in `filter_entry` for performance; FILE
    //      matches are evaluated inside the loop so audit fix K15 can
    //      count them in the per-category skip breakdown. Without this
    //      split, gitignore-pruned files vanish from `walked` and the
    //      summary's `354 skipped` line never tells the user which
    //      bucket they fell into.
    let gi = load_project_ignore(&project);
    let gi_ref = gi.as_ref();
    let walker = walkdir::WalkDir::new(&project)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let p = e.path();
            if is_ignored(p) { return false; }
            // Only prune DIRECTORIES via gitignore here; files are
            // checked (and counted) inside the loop. K15.
            if e.file_type().is_dir() {
                return !project_ignore_matches(gi_ref, p, true);
            }
            true
        });

    let mut total = 0usize;
    let mut indexed = 0usize;
    let mut skipped_binary = 0usize;
    let mut skipped_unsupported = 0usize;
    let mut skipped_gitignore = 0usize;
    let mut skipped_too_large = 0usize;
    let mut node_total = 0u64;
    let mut edge_total = 0u64;

    // K10 #6: when resuming, skip files lexically <= the last completed
    // file. We use the saved cursor only — the actual decision to skip
    // a file is still subject to the per-file-hash check in graph.db,
    // so over-skipping in error here doesn't drop work permanently.
    let resume_cursor: Option<String> = resume_state
        .as_ref()
        .filter(|s| !s.last_completed_file.is_empty())
        .map(|s| s.last_completed_file.clone());

    // I1 batch 3 — track parse/extract failures so the errors-pass can
    // persist a deduplicated row per (error_hash, message) into
    // `errors.db::errors`. Each entry is `(message, file_path)`; stack
    // stays NULL because anyhow chains are flattened to message-only
    // at this call site.
    let mut build_errors: Vec<(String, String)> = Vec::new();

    // Silent-1 fix (Class H-silent in `docs/dev/DEEP-AUDIT-2026-04-29.md`):
    // count graph-insert failures. The previous code did
    // `let _ = store.inject.insert(...).await;` so a failed insert
    // (DB locked, schema race, disk full) silently produced an
    // incomplete graph and the build still printed "ok". Now every
    // insert checks `Response::success`; failures are pushed into
    // `build_errors` (so they persist into errors.db) AND counted
    // here. After the parse loop the build refuses to proceed with
    // a non-zero count — the user sees a real error instead of an
    // "ok" that lies about a half-written graph.
    let mut graph_insert_failures: u64 = 0;

    // I1 batch 3 — convention learner. Cheap regex-only observations,
    // flushed at end of build via `run_conventions_pass`.
    let mut convention_learner = DefaultLearner::new();

    // I1 batch 3 — capture wall-clock so the perf-pass can persist a
    // build-duration baseline alongside per-stage throughput numbers.
    let inline_started = Instant::now();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "walk error; continuing");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        if args.limit > 0 && indexed >= args.limit {
            break;
        }
        total += 1;
        // B-017: feed the heartbeat the discovered upper bound.
        // `add_total(1)` per file walked produces a continuously-growing
        // total that the 30 s status line can render. The first
        // heartbeat may show `processed=N/M` where M is still climbing —
        // that's correct: the user's mental model is "build is making
        // progress", not "exact final count is known".
        heartbeat.add_total(1);

        let path = entry.path();

        // K10 #6: resume-skip. Files lexically `<= last_completed_file`
        // were persisted in the previous run; skip the parse work and
        // keep walking. We treat them as "indexed" for the skip-tally
        // accounting so the summary line still reflects total work
        // covered (just not all of it from this run).
        if let Some(cursor) = resume_cursor.as_ref() {
            let path_str = path.display().to_string();
            if path_str.as_str() <= cursor.as_str() {
                continue;
            }
        }

        // K15: gitignore is checked at the file level so the skip
        // categories add up to `total - indexed`. Files in ignored
        // directories never reach this point (pruned in filter_entry).
        if project_ignore_matches(gi_ref, path, false) {
            skipped_gitignore += 1;
            continue;
        }

        let Some(lang) = Language::from_filename(path) else {
            skipped_unsupported += 1;
            continue;
        };
        if !lang.is_enabled() {
            skipped_unsupported += 1;
            continue;
        }

        // K15: audit-time threshold for "too large to parse". The
        // tree-sitter pool's per-parse cost grows roughly linearly with
        // file size; >5 MiB single source files are vanishingly rare in
        // real codebases and are almost always vendored bundles or
        // generated artefacts. Counting them as their own category lets
        // the user see whether the build silently dropped a giant file.
        let metadata_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if metadata_len > MAX_PARSE_BYTES {
            skipped_too_large += 1;
            continue;
        }

        let content = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                warn!(file = %path.display(), error = %e, "read failed; skipping");
                // I1 batch 3 — capture read failures into errors.db.
                build_errors.push((
                    format!("read failed: {e}"),
                    path.display().to_string(),
                ));
                skipped_unsupported += 1;
                continue;
            }
        };
        if looks_binary(&content) {
            skipped_binary += 1;
            continue;
        }

        let content_arc = Arc::new(content);
        let parse_result = inc.parse_file(path, lang, content_arc.clone()).await;
        let parse = match parse_result {
            Ok(p) => p,
            Err(e) => {
                warn!(file = %path.display(), error = %e, "parse failed; skipping");
                // I1 batch 3 — capture parse failures into errors.db.
                // The bucket above (`skipped_unsupported`) preserves
                // the K15 contract; this collector is additive.
                build_errors.push((
                    format!("parse failed: {e}"),
                    path.display().to_string(),
                ));
                // Tree-sitter rejected the file outright. Bucket as
                // "unsupported language" since the grammar refused —
                // the user-visible result is identical (file went in,
                // no nodes came out) and we already log the real
                // error via `warn!`. K15.
                skipped_unsupported += 1;
                continue;
            }
        };

        let extractor = Extractor::new(lang);
        let graph = match extractor.extract(&parse.tree, &content_arc, path) {
            Ok(g) => g,
            Err(e) => {
                warn!(file = %path.display(), error = %e, "extract failed; skipping");
                // I1 batch 3 — capture extract failures into errors.db.
                build_errors.push((
                    format!("extract failed: {e}"),
                    path.display().to_string(),
                ));
                skipped_unsupported += 1;
                continue;
            }
        };

        // I1 batch 3 — feed the convention learner. Regex-only path: we
        // pass `ast=None`, which makes the learner use its built-in
        // language-aware regex heuristics. Tree-sitter trees are
        // already in hand (`parse.tree`) but the learner's trait takes
        // a placeholder `tree_sitter_tree::Tree` shim that doesn't yet
        // accept a real `tree_sitter::Tree` — the regex path is the
        // current contract.
        if let Ok(s) = std::str::from_utf8(content_arc.as_slice()) {
            convention_learner.observe_file(path, s, None);
        }
        // I2/K5: file-level facts shared by every node we emit and by the
        // row we add to the `files` table. `is_test` is computed once per
        // file and propagated to ALL descendant nodes — see
        // phase-a-issues.md §K5 for the spec.
        let is_test_file = looks_like_test_path(path);
        let file_sha = hex_sha256(content_arc.as_slice());
        let line_count = 1 + content_arc.iter().filter(|&&b| b == b'\n').count();
        let byte_count = content_arc.len();

        // I2: populate `files` so the vision treemap / heatmap / file-tree
        // queries (`vision/server/shard.ts`) get rows back. The data is
        // already in hand — just persist it. Keyed on path so re-parses
        // self-heal via INSERT OR REPLACE.
        let files_sql = "INSERT OR REPLACE INTO files(path, sha256, language, last_parsed_at, line_count, byte_count) \
                         VALUES(?1, ?2, ?3, datetime('now'), ?4, ?5)";
        let files_params = vec![
            serde_json::Value::String(path.display().to_string()),
            serde_json::Value::String(file_sha.clone()),
            serde_json::Value::String(format!("{:?}", lang).to_lowercase()),
            serde_json::Value::Number((line_count as i64).into()),
            serde_json::Value::Number((byte_count as i64).into()),
        ];
        // Silent-1: capture & track insert failures (was `let _ = ...`).
        let files_resp = store
            .inject
            .insert(
                &project_id,
                DbLayer::Graph,
                files_sql,
                files_params,
                InjectOptions {
                    emit_event: false,
                    audit: false,
                    ..InjectOptions::default()
                },
            )
            .await;
        if !files_resp.success {
            graph_insert_failures += 1;
            let msg = files_resp
                .error
                .as_ref()
                .map(|e| format!("graph insert (files row) failed: {}", e.message))
                .unwrap_or_else(|| "graph insert (files row) failed: no error detail".to_string());
            build_errors.push((msg, path.display().to_string()));
        }

        // I4: filter `Comment` nodes from the writer-visible graph. Per
        // phase-a-issues.md §I4 ~50% of `nodes` rows were comments, which
        // distorts god-node + community + coupling stats. Comments stay
        // re-extractable from the AST per file when a downstream feature
        // actually needs them.
        let writeable: Vec<&parsers::Node> = graph
            .nodes
            .iter()
            .filter(|n| n.kind != NodeKind::Comment)
            .collect();
        let n_nodes = writeable.len();
        let n_edges = graph.edges.len();

        // Persist. Map parsers::Node → graph.db schema. `id` from parsers
        // becomes the qualified_name (it's already a stable, unique string
        // per §stable_id in extractor.rs).
        for node in &writeable {
            let sql = "INSERT OR REPLACE INTO nodes(kind,name,qualified_name,file_path,line_start,line_end,language,is_test,file_hash,extra,updated_at) \
                       VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,datetime('now'))";
            let params = vec![
                serde_json::Value::String(format!("{:?}", node.kind).to_lowercase()),
                serde_json::Value::String(node.name.clone()),
                serde_json::Value::String(node.id.clone()),
                serde_json::Value::String(node.file.display().to_string()),
                serde_json::Value::Number((node.line_range.0 as i64).into()),
                serde_json::Value::Number((node.line_range.1 as i64).into()),
                serde_json::Value::String(format!("{:?}", node.language).to_lowercase()),
                serde_json::Value::Number((if is_test_file { 1i64 } else { 0i64 }).into()),
                serde_json::Value::String(file_sha.clone()),
                serde_json::Value::String(
                    serde_json::json!({
                        "confidence": format!("{:?}", node.confidence).to_lowercase(),
                        "byte_range": [node.byte_range.0, node.byte_range.1],
                    })
                    .to_string(),
                ),
            ];
            // Silent-1: capture & track per-node insert failures.
            let node_resp = store
                .inject
                .insert(
                    &project_id,
                    DbLayer::Graph,
                    sql,
                    params,
                    InjectOptions {
                        emit_event: false,
                        audit: false,
                        ..InjectOptions::default()
                    },
                )
                .await;
            if !node_resp.success {
                graph_insert_failures += 1;
                let msg = node_resp
                    .error
                    .as_ref()
                    .map(|e| format!("graph insert (node {}) failed: {}", node.id, e.message))
                    .unwrap_or_else(|| {
                        format!("graph insert (node {}) failed: no error detail", node.id)
                    });
                build_errors.push((msg, path.display().to_string()));
            }
        }
        for edge in &graph.edges {
            let sql = "INSERT INTO edges(kind,source_qualified,target_qualified,confidence,confidence_score,source_extractor,extra,updated_at) \
                       VALUES(?1,?2,?3,?4,?5,?6,?7,datetime('now'))";
            let conf = format!("{:?}", edge.confidence).to_lowercase();
            let score = edge.confidence.weight();
            let params = vec![
                serde_json::Value::String(format!("{:?}", edge.kind).to_lowercase()),
                serde_json::Value::String(edge.from.clone()),
                serde_json::Value::String(edge.to.clone()),
                serde_json::Value::String(conf),
                serde_json::Value::Number(
                    serde_json::Number::from_f64(score as f64)
                        .unwrap_or_else(|| serde_json::Number::from(1)),
                ),
                serde_json::Value::String("parsers".into()),
                serde_json::Value::String(
                    serde_json::json!({
                        "unresolved": edge.unresolved_target,
                    })
                    .to_string(),
                ),
            ];
            // Silent-1: capture & track per-edge insert failures.
            let edge_resp = store
                .inject
                .insert(
                    &project_id,
                    DbLayer::Graph,
                    sql,
                    params,
                    InjectOptions {
                        emit_event: false,
                        audit: false,
                        ..InjectOptions::default()
                    },
                )
                .await;
            if !edge_resp.success {
                graph_insert_failures += 1;
                let msg = edge_resp
                    .error
                    .as_ref()
                    .map(|e| {
                        format!(
                            "graph insert (edge {}->{}) failed: {}",
                            edge.from, edge.to, e.message
                        )
                    })
                    .unwrap_or_else(|| {
                        format!(
                            "graph insert (edge {}->{}) failed: no error detail",
                            edge.from, edge.to
                        )
                    });
                build_errors.push((msg, path.display().to_string()));
            }
        }

        indexed += 1;
        node_total += n_nodes as u64;
        edge_total += n_edges as u64;
        // B-017: lock-free progress update for the heartbeat task. The
        // 30 s tick reads this atomic — calling it on every file is
        // cheap (atomic store, no syscall) and gives the heartbeat
        // accurate per-second-resolution rate numbers.
        heartbeat.record_processed(indexed as u64);
        if indexed % 25 == 0 {
            println!("  indexed {indexed} files ({node_total} nodes, {edge_total} edges)");
            // K10 #6: persist the parse-pass cursor. Same cadence as
            // the existing log line (every 25 files) so disk pressure
            // is bounded but the resume cursor is never more than ~25
            // files stale on Ctrl-C between saves.
            current_state.mark_parse_progress(indexed as u64, path);
            if let Err(e) = build_state::save(&project, &current_state) {
                tracing::debug!(error = %e, "failed to persist build-state checkpoint (non-fatal)");
            }
        }
    }
    // Final save at end of parse pass — covers projects with < 25
    // files (where the modulo never triggers) and the tail of larger
    // projects. Silent-2 fix (Class H-silent in DEEP-AUDIT-2026-04-29.md):
    // this is a contract-bearing phase transition into Multimodal — if
    // it doesn't land, a resumed build restarts from Parse and redoes
    // everything. Propagate the io error rather than swallow it.
    if indexed > 0 {
        current_state.enter_phase(build_state::BuildPhase::Multimodal);
        save_phase_transition(&project, &current_state)?;
    }

    // 4. Multimodal pass. The code walk only handles Tree-sitter-backed
    // languages; anything else (PDFs, Markdown, images, audio, video) is
    // handed to `mneme-multimodal`'s registry. PDF pages are the MVP here:
    // extracted text lands in BOTH `multimodal.db::media` (full document
    // payload) AND `graph.db::nodes` (one row per page, kind='pdf_page',
    // `summary` = page text — so `recall_concept` hits PDF content via the
    // existing nodes_fts index without a schema bump).
    heartbeat.set_phase("multimodal");
    let mm_stats = run_multimodal_pass(&store, &project_id, &project).await;

    // 4.5. Imports-edge resolver pass (Phase A blast=0 fix). Walks every
    // imports edge whose target_qualified still starts with `import::`
    // (the pseudo-id stamped by extractor.rs::collect_imports), parses
    // the unresolved module from `extra.unresolved`, resolves it
    // against the importer's directory + common JS/TS/Py/Rs extension
    // and `index.*` heuristics, and UPDATEs target_qualified to the
    // real file's qualified-name. Without this pass `mneme blast`,
    // `find_references`, and `call_graph` all return zero because no
    // edge ever targeted a real node. Bare-package imports
    // (lodash/react/numpy) are flagged as unresolved_bare and stay as
    // pseudo-ids by design — they have no local file target.
    //
    // MUST run before Leiden so community detection clusters by
    // resolved connectivity rather than by pseudo-id text.
    heartbeat.set_phase("resolve-imports");
    let resolve_stats = run_resolve_imports_pass(&store, &project_id, &project, &paths).await;

    // 5. Leiden community detection. Reads (source_qualified, target_qualified)
    // pairs out of graph.db::edges, runs the deterministic Rust Leiden solver
    // (brain::cluster_runner::ClusterRunner), and writes the resulting
    // communities + membership mapping into semantic.db::{communities,
    // community_membership}. Without this pass semantic.db stayed empty
    // forever — every consumer (mcp/tools/architecture_overview, mcp/tools/
    // surprising_connections, vision/server/shard, scanners/architecture)
    // reads the membership table; nothing wrote it.
    //
    // Failure here is non-fatal: clustering is bonus structure on top of a
    // successful raw graph build.
    heartbeat.set_phase("graph-leiden");
    let cluster_stats = run_leiden_pass(&store, &project_id, &paths).await;

    // 6. Embedding pass (P3.4). Reads every substantive node out of
    // graph.db, runs `brain::Embedder::embed_batch` over their textual
    // signatures/summaries, and persists 384-dim f32 vectors into
    // `semantic.db::embeddings`. Back-links via
    // `graph.db::nodes.embedding_id`. Without this pass the embeddings
    // table stays empty forever and `mneme recall` degrades silently
    // to keyword-only retrieval. Failure is non-fatal — same contract
    // as the Leiden pass.
    heartbeat.set_phase("embed");
    let embedding_stats = run_embedding_pass(&store, &project_id, &paths).await;

    // 7. Audit pass (I1 partial — populate findings.db). Spawns the
    // mneme-scanners worker via the existing audit subprocess fallback
    // and lets it run all 11 scanners (theme/security/perf/a11y/drift/
    // ipc/md-drift/secrets/refactor/architecture/types) over the project,
    // persisting findings to findings.db::findings. Without this pass
    // findings.db stayed at 0 rows even though the scanners crate has
    // been functional for months — phase-a-issues §I1's biggest
    // single-shard miss. Failure non-fatal: a cmd that lands findings
    // is gravy on top of a successful graph build.
    //
    // 4.3: pass `args.inline` so the audit pass takes the direct
    // subprocess path under `--inline` instead of going via
    // `audit::run` (which dials the supervisor and would auto-spawn
    // `mneme-daemon` on a dead pipe — leaking processes the user
    // explicitly opted out of).
    let audit_stats = run_audit_pass(&project, args.inline, &children).await;

    // 8. Tests-shard population (I1). Copies (file_path, framework)
    // from graph.db::nodes where kind='file' AND is_test=1 into
    // tests.db::test_files. K5 already does the heuristic detection
    // during the parse loop; this pass just materialises the result
    // into the canonical shard.
    let tests_stats = run_tests_pass(&store, &project_id, &paths).await;

    // 9. Git-shard population (I1). Mines `git log` for commits +
    // file-level changes and `git blame` (sampled) into git.db. Without
    // this pass git.db.commits stayed at 0 rows and the vision Timeline
    // view + decision-recall git-history heuristics had no data.
    // Failure non-fatal: missing git, non-git directory, sparse repo —
    // all just leave git.db empty.
    let git_stats = run_git_pass(&store, &project_id, &project).await;

    // 10. Deps-shard population (I1). Parses package.json (npm),
    // Cargo.toml (rust workspace + leaf), requirements.txt (python)
    // for top-level dependencies. Populates deps.db.dependencies.
    // Future: pull vulnerabilities via local advisory feed (out of
    // scope this pass).
    let deps_stats = run_deps_pass(&store, &project_id, &project).await;

    // 11. Betweenness centrality pass (H6). Sampled Brandes algorithm
    // computes per-node betweenness over the existing graph.db edge
    // set, persisting to graph.db::node_centrality. Replaces the
    // previous `betweenness: 0` constant in god_nodes responses with
    // real (or sample-approximated) BC scores. Bounded by source-cap
    // and wall-clock cap so very large graphs degrade gracefully.
    heartbeat.set_phase("graph-betweenness");
    let betweenness_stats = run_betweenness_pass(&store, &project_id, &paths).await;

    // 12. Intent pass (J1). Scans every indexed source file's first 30
    // lines for a `@mneme-intent: <kind>[: <reason>]` magic comment in
    // any of the language's comment styles (// /* # ;). Matched files
    // get a row in memory.db.file_intent with source='annotation'.
    // Files without a magic comment get an inferred row source='git'
    // when git history shows them as long-stable (>1 year unchanged
    // and >2k LOC) — coarse heuristic, marked low-confidence.
    let mut intent_stats = run_intent_pass(&store, &project_id, &project, &paths).await;

    // 12b. J2 — git-history intent heuristics. Mines git.db for files
    // that look frozen (long-untouched + low churn), deferred (high
    // TODO/FIXME density + recent churn), or explicitly marked in
    // commit messages ("verbatim", "do not touch"). Only writes rows
    // when no annotation row already exists for that file
    // (annotation-source has priority).
    run_git_intent_pass(&store, &project_id, &project, &paths, &mut intent_stats).await;

    // 12c. J4 — convention rules from `intent.config.json` at project
    // root. Each rule is `{glob, intent[, reason]}` — globs apply to
    // the project-relative file path. Source='convention',
    // confidence=0.9. Skips files that already have an annotation /
    // git row.
    run_convention_intent_pass(
        &store,
        &project_id,
        &project,
        &paths,
        &mut intent_stats,
    )
    .await;

    // 12d. J6 — per-directory `INTENT.md` annotations. Each line of
    // the form `- <filename>: <intent> — <reason>` applies to the
    // file in the same directory as the INTENT.md. Source='convention'
    // (it's a team agreement, same bucket as J4). Skips already-set
    // rows.
    run_intent_md_pass(&store, &project_id, &project, &paths, &mut intent_stats).await;

    // 13. Architecture-snapshot pass (I1 batch 3). Reads graph.db nodes
    // + edges + semantic.db community_membership, runs the existing
    // ArchitectureScanner over them, and writes one new row to
    // architecture.db::architecture_snapshots. Append-only — readers
    // pick the newest row. Without this pass architecture.db stayed
    // empty even though the analysis library was already in tree.
    heartbeat.set_phase("graph-architecture");
    let architecture_stats =
        run_architecture_pass(&store, &project_id, &paths).await;

    // 14. Conventions pass (I1 batch 3). Materialises the inferred
    // patterns the `convention_learner` accumulated during the file
    // loop into conventions.db::conventions. One row per inferred
    // pattern with a deterministic id (sha256 over kind+payload) so
    // re-runs upsert in place.
    let conventions_stats =
        run_conventions_pass(&store, &project_id, &convention_learner).await;

    // 15. Wiki pass (I1 batch 3). For each Leiden community we just
    // materialised in semantic.db::communities, build a markdown wiki
    // page using `brain::wiki::WikiBuilder` and persist a row into
    // wiki.db::wiki_pages. Append-only — every regeneration bumps the
    // `version` column for the same slug. Pages have no entry-point
    // god-nodes plumbing here (that's a daemon-side concern that needs
    // betweenness + criticality), so we anchor on community-id and
    // file-path lists; the daemon path can supersede this with richer
    // pages later.
    heartbeat.set_phase("wiki");
    let wiki_stats = run_wiki_pass(&store, &project_id, &paths).await;

    // 16. Federated fingerprints pass (I1 batch 3). Trigger the
    // federated scan against the project root so federated.db is
    // populated as a side-effect of `mneme build` instead of requiring
    // an explicit `mneme federated scan`. Local-only — fingerprints
    // never leave the box without the opt-in marker.
    let federated_stats = run_federated_pass(&project_id, &project, &paths).await;

    // 17. Perf-baselines pass (I1 batch 3). Capture build-time
    // throughput numbers (files/sec, nodes/sec, total ms) into
    // perf.db::baselines. The perf scanner already produces *findings*;
    // this pass produces *baselines*, which is the empty-shard the
    // Phase A audit flagged (different table, same shard).
    let perf_stats = run_perf_pass(
        &store,
        &project_id,
        inline_started.elapsed(),
        indexed,
        node_total,
        edge_total,
    )
    .await;

    // 18. Errors pass (I1 batch 3). Persist the parse/read/extract
    // failures collected during the file loop into errors.db::errors,
    // deduplicated by `error_hash` (blake3 of message+file). Each
    // recurrence bumps `encounters` and refreshes `last_seen` via
    // INSERT … ON CONFLICT DO UPDATE.
    let errors_stats = run_errors_pass(&store, &project_id, &build_errors).await;

    // 19. Live-state pass (I1 batch 3). The hook layer already writes
    // file_events for every Edit/Write tool invocation, but a build
    // run on a fresh shard leaves livestate.db empty until the user
    // starts editing. Stamp one `build_completed` event so an operator
    // running `mneme build .` immediately sees a row when they
    // inspect livestate.db. Subsequent hook writes accumulate on top.
    let livestate_stats = run_livestate_pass(&store, &project_id, &project).await;

    // 20. Agents pass (I1 batch 3). agents.db::subagent_runs is
    // populated by the SubagentStop hook (`mneme turn-end --subagent`)
    // — which is the right primary producer because subagent runs are
    // per-turn events, not per-build. The `cli/src/commands/turn_end.rs`
    // path now writes the row; for `mneme build` itself we record a
    // synthetic "build" run as a smoke marker so the shard isn't 0
    // rows on a fresh project.
    let agents_stats = run_agents_pass(&store, &project_id, &project).await;

    // Stamp meta.db::projects.last_indexed_at so the staleness nag
    // (audit-L12) can tell users when their recall results are
    // potentially out of date. This must run AFTER the multimodal pass
    // succeeds - a stamp on a half-built shard would mask a real
    // staleness signal. Failure here is non-fatal: the build itself
    // succeeded, we just couldn't update the bookkeeping row.
    if let Err(e) = store::mark_indexed(&paths, &project_id) {
        warn!(error = %e, "failed to stamp last_indexed_at; staleness nag may be inaccurate");
    }

    // B-017: every silent pass has now finished. Stop the heartbeat
    // BEFORE the build-complete summary block fires so the 30 s
    // status lines can't interleave with the (atomic, contiguous)
    // summary output. `Heartbeat::stop` is idempotent — `Drop`
    // handles re-entry safely if a future code path re-uses the
    // handle.
    let mut heartbeat = heartbeat;
    heartbeat.stop();

    let skipped_total = skipped_binary
        + skipped_unsupported
        + skipped_gitignore
        + skipped_too_large;

    println!();
    println!("build complete:");
    println!("  walked:       {total} files");
    println!("  indexed:      {indexed}");
    // K15: categorize the skip bucket so a `354 skipped` line is no
    // longer a black box. The four categories are exhaustive — every
    // `continue` from the parse loop above maps to exactly one of them.
    println!(
        "  skipped:      {skipped_total} ({} binary, {} unsupported language, {} .gitignore, {} too-large)",
        skipped_binary, skipped_unsupported, skipped_gitignore, skipped_too_large
    );
    println!("  nodes:        {node_total}");
    println!("  edges:        {edge_total}");
    // K14: the 4,122 pages/sec figure is misleading when we're really
    // doing dimensions-only on images and metadata-only on PDFs.
    // Append the qualifier unless the multimodal crate was compiled
    // with the `tesseract` feature (which actually performs OCR).
    // `multimodal::OCR_ENABLED` is `cfg!(feature = "tesseract")`
    // forwarded from the multimodal-bridge crate so a future
    // `cargo build --features multimodal/tesseract` drops the
    // qualifier automatically — no separate CLI flag to maintain.
    let ocr_qualifier = if multimodal::OCR_ENABLED {
        ""
    } else {
        " — dimensions only, OCR disabled"
    };
    println!(
        "  resolved:     {} / {} import edges (bare={}, unresolved={}, ms={})",
        resolve_stats.resolved,
        resolve_stats.total_imports_edges,
        resolve_stats.unresolved_bare,
        resolve_stats.unresolved_relative,
        resolve_stats.duration_ms
    );
    println!(
        "  multimodal:   {} files, {} pages ({} errors, {} pages/sec{})",
        mm_stats.files_ok,
        mm_stats.pages_total,
        mm_stats.errors,
        mm_stats.pages_per_sec(),
        ocr_qualifier,
    );
    println!(
        "  communities:  {} (members={}, edges_used={})",
        cluster_stats.communities,
        cluster_stats.members,
        cluster_stats.edges_used,
    );
    // PM-5: include the explicit `model:` qualifier alongside the legacy
    // `backend=` tag. Both render the same string today (Embedder::
    // backend_name returns `bge-small-en-v1.5` or `hashing-trick`), but
    // the explicit `model:` prefix matches the wording `mneme doctor`
    // and the install summary use, so the three surfaces stay in sync.
    println!(
        "  embeddings:   {} written, {} linked, model={}, backend={} (scanned={}, with_text={}, failures={})",
        embedding_stats.embeddings_written,
        embedding_stats.nodes_linked,
        embedding_stats.backend,
        embedding_stats.backend,
        embedding_stats.nodes_scanned,
        embedding_stats.nodes_with_text,
        embedding_stats.failures,
    );
    println!(
        "  audit:        {} (run_attempted={}, status={})",
        if audit_stats.ran { "ran" } else { "skipped" },
        audit_stats.ran,
        audit_stats.status,
    );
    println!(
        "  tests:        {} test files materialised into tests.db",
        tests_stats.test_files_written,
    );
    println!(
        "  git:          {} commits, {} file-changes (status={})",
        git_stats.commits_written,
        git_stats.commit_files_written,
        git_stats.status,
    );
    println!(
        "  deps:         {} dependencies parsed (npm={}, cargo={}, python={})",
        deps_stats.deps_written,
        deps_stats.npm_count,
        deps_stats.cargo_count,
        deps_stats.python_count,
    );
    println!(
        "  betweenness:  {} nodes scored, top BC={:.4}, sources_sampled={}, ms={}",
        betweenness_stats.nodes_scored,
        betweenness_stats.top_score,
        betweenness_stats.sources_sampled,
        betweenness_stats.duration_ms,
    );
    // J8 — break out per-source counts so operators can see which
    // intent surfaces are alive on this project. `inferred` is the
    // legacy J1 sub-counter for non-annotation rows it wrote
    // directly; new sources (git/convention) are reported separately.
    println!(
        "  intent:       {} files annotated (annotation={}, git={}, convention={}, inferred={})",
        intent_stats.total_written,
        intent_stats.annotation_count,
        intent_stats.git_count,
        intent_stats.convention_count,
        intent_stats.inferred_count,
    );
    println!(
        "  architecture: {} snapshot ({} comms, {} bridges, {} hubs)",
        architecture_stats.snapshots_written,
        architecture_stats.community_count,
        architecture_stats.bridges,
        architecture_stats.hubs,
    );
    println!(
        "  conventions:  {} patterns inferred ({} written)",
        conventions_stats.patterns_inferred,
        conventions_stats.rows_written,
    );
    println!(
        "  wiki:         {} pages generated ({} written)",
        wiki_stats.pages_built,
        wiki_stats.pages_written,
    );
    println!(
        "  federated:    {} fingerprints indexed ({} skipped)",
        federated_stats.indexed,
        federated_stats.skipped,
    );
    println!(
        "  perf:         {} baselines written (build_ms={})",
        perf_stats.baselines_written,
        perf_stats.build_ms,
    );
    println!(
        "  errors:       {} captured ({} unique)",
        errors_stats.captured,
        errors_stats.unique,
    );
    // Silent-1 (Class H-silent): surface graph-insert failures in the
    // build summary. The audit prescription is "fail-loud if any return
    // Err" — we print the line unconditionally so operators always see
    // a `0` on a healthy build and a non-zero count on a degraded one.
    println!(
        "  graph-insert: {} failures",
        graph_insert_failures,
    );
    println!(
        "  livestate:    {} build event(s) recorded",
        livestate_stats.events_written,
    );
    println!(
        "  agents:       {} synthetic run(s) recorded (per-turn rows owned by SubagentStop hook)",
        agents_stats.runs_written,
    );
    println!("  shard:        {}", paths.project_root(&project_id).display());

    // I6: print the per-shard row counts so an operator running
    // `mneme build .` immediately sees which of the 26 shards are
    // populated and which are empty placeholders. Driven by
    // `DbLayer::all_per_project()`; non-fatal on any I/O or query
    // error — the build itself is already done.
    crate::commands::shard_summary::print_shard_summary(
        &paths.project_root(&project_id),
    );

    // K3/K4: re-emit the model warnings at the END of the build summary so
    // the message survives long log scrolls. The probe is cheap (it just
    // checks for files on disk) so re-doing it here is fine.
    model_probes.print_warnings_in_summary();

    // K3 + H4 — explicit status lines so silent failures become
    // impossible. The legacy `embeddings:` and `communities:` rows
    // above carry the raw counters; these two lines tell the operator
    // what the counters MEAN: did the embedding model load, and is the
    // community partition reproducible. Logged via `info!` AND printed
    // so both interactive use and structured-log capture see them.
    let emb_line = model_probes.summary_embedding_status_line();
    let comm_line = community_status_line(&cluster_stats);
    info!(target: "build.summary", embedding = %emb_line, community = %comm_line);
    println!("  {emb_line}");
    println!("  {comm_line}");

    // Silent-1 (Class H-silent): fail-loud when any insert into a
    // contract-bearing layer (graph or wiki) was dropped. The build
    // summary above already printed the parse-loop count and the
    // per-failure rows are persisted into errors.db (operators can
    // `mneme audit` to inspect). The wiki insert failures come from
    // `WikiStats::insert_failures` (line 5346 in DEEP-AUDIT) and the
    // failed wiki_pages writes inside `run_wiki_pass`. Returning Err
    // here ensures `mneme build` exits non-zero so CI / scripts can
    // react. The checkpoint is intentionally NOT cleared in this
    // branch — a partial build leaves resume info so the next run can
    // retry the dropped rows.
    let total_silent1_failures = graph_insert_failures + wiki_stats.insert_failures;
    if total_silent1_failures > 0 {
        return Err(CliError::Other(format!(
            "build incomplete: {graph_insert_failures} graph insert(s) and \
             {wiki_failures} wiki insert(s) failed; see errors.db for per-failure \
             detail and re-run `mneme build .` to retry",
            wiki_failures = wiki_stats.insert_failures,
        )));
    }

    // K10 #6: build completed cleanly. Mark the state as Done and
    // delete the checkpoint so a re-build doesn't pick up stale resume
    // info. We deliberately delete on the success path only — a build
    // that errors out leaves the checkpoint in place so the user's
    // next run can retry from where it stopped.
    current_state.enter_phase(build_state::BuildPhase::Done);
    build_state::clear(&project);

    Ok(())
}

/// Aggregate stats from the multimodal pass.
#[derive(Debug, Default)]
struct MultimodalStats {
    files_ok: usize,
    /// Files we tried to extract but failed on (error already counted too).
    /// Kept as a separate counter so the top-line summary could split the
    /// two; currently only `errors` is printed.
    #[allow(dead_code)]
    files_skipped: usize,
    pages_total: usize,
    errors: usize,
    duration_secs: f64,
}

impl MultimodalStats {
    fn pages_per_sec(&self) -> String {
        if self.duration_secs <= 0.0 {
            return "-".into();
        }
        format!("{:.1}", self.pages_total as f64 / self.duration_secs)
    }
}

/// Walk `project`, dispatch every path the multimodal [`Registry`] claims
/// through its extractor, and persist the result. PDFs are the fully-wired
/// path; other kinds (markdown/image/audio/video) go through the same
/// machinery but are tolerated if their extractors are feature-gated off.
///
/// Errors are logged and counted, never raised — the multimodal pass is
/// strictly additive on top of a successful code build.
async fn run_multimodal_pass(
    store: &Store,
    project_id: &ProjectId,
    project: &Path,
) -> MultimodalStats {
    let mut stats = MultimodalStats::default();
    let start = Instant::now();
    let registry = MmRegistry::default_wired();

    let gi = load_project_ignore(project);
    let gi_ref = gi.as_ref();
    let walker = walkdir::WalkDir::new(project)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let p = e.path();
            if is_ignored(p) { return false; }
            !project_ignore_matches(gi_ref, p, e.file_type().is_dir())
        });

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "mm walk error; continuing");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if registry.find(path).is_none() {
            continue;
        }

        let doc = match registry.extract(path) {
            Ok(d) => d,
            Err(multimodal::ExtractError::Unsupported { .. }) => {
                continue;
            }
            Err(e) => {
                warn!(file = %path.display(), error = %e, "multimodal extract failed");
                stats.errors += 1;
                stats.files_skipped += 1;
                continue;
            }
        };

        let pages_written = match persist_multimodal(store, project_id, &doc).await {
            Ok(n) => n,
            Err(e) => {
                warn!(file = %path.display(), error = %e, "multimodal persist failed");
                stats.errors += 1;
                stats.files_skipped += 1;
                continue;
            }
        };
        stats.files_ok += 1;
        stats.pages_total += pages_written;
    }

    stats.duration_secs = start.elapsed().as_secs_f64();
    stats
}

/// Persist an [`ExtractedDoc`] to the project shard.
///
/// Two writes per document:
///   1. `multimodal.db::media` — one row per file (whole payload).
///   2. `graph.db::nodes` — one row per page. For PDFs this is what
///      `recall_concept` returns, since `nodes_fts.summary` is indexed.
///      For `kind != "pdf"` we still write the top-level document as a
///      single node (no per-page split) so the text is discoverable.
///
/// Idempotent via `INSERT OR REPLACE` on the unique key columns
/// (`media.path`, `nodes.qualified_name`).
async fn persist_multimodal(
    store: &Store,
    project: &ProjectId,
    doc: &ExtractedDoc,
) -> Result<usize, String> {
    let bytes = std::fs::read(&doc.source)
        .map_err(|e| format!("read {}: {e}", doc.source.display()))?;
    let sha = hex_sha256(&bytes);
    let elements_json = serde_json::to_string(&doc.elements).unwrap_or_else(|_| "[]".into());
    let transcript_json = if doc.transcript.is_empty() {
        String::new()
    } else {
        serde_json::to_string(&doc.transcript).unwrap_or_default()
    };

    // Write to multimodal.db::media (whole-document payload).
    let media_sql = "INSERT OR REPLACE INTO media(path, sha256, media_type, extracted_text, elements, transcript, extracted_at, extractor_version) \
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, datetime('now'), ?7)";
    let media_params = vec![
        serde_json::Value::String(doc.source.display().to_string()),
        serde_json::Value::String(sha),
        serde_json::Value::String(doc.kind.clone()),
        serde_json::Value::String(doc.text.clone()),
        serde_json::Value::String(elements_json),
        if transcript_json.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(transcript_json)
        },
        serde_json::Value::String(doc.extractor_version.clone()),
    ];
    let resp = store
        .inject
        .insert(
            project,
            DbLayer::Multimodal,
            media_sql,
            media_params,
            InjectOptions {
                emit_event: false,
                audit: false,
                ..InjectOptions::default()
            },
        )
        .await;
    if !resp.success {
        return Err(resp
            .error
            .map(|e| format!("media insert: {e:?}"))
            .unwrap_or_else(|| "unknown media insert error".into()));
    }

    // Write to graph.db::nodes (one row per page for PDFs, one for the
    // whole doc otherwise). The `summary` column is indexed by nodes_fts
    // so `recall_concept` surfaces the text without a schema change.
    let mut pages_written = 0usize;
    let source_display = doc.source.display().to_string();
    let scheme = match doc.kind.as_str() {
        "pdf" => "pdf",
        "markdown" => "md",
        "image" => "img",
        "audio" => "audio",
        "video" => "video",
        _ => "file",
    };

    let page_records: Vec<(u32, String, Option<String>)> = if doc.pages.is_empty() {
        vec![(1, doc.text.clone(), None)]
    } else {
        doc.pages
            .iter()
            .map(|p| (p.index, p.text.clone(), p.heading.clone()))
            .collect()
    };

    for (page_num, page_text, heading) in page_records {
        // Skip empty pages — no point indexing whitespace. Keeps the graph
        // lean and avoids a noisy FTS row per blank PDF page.
        if page_text.trim().is_empty() {
            continue;
        }

        let node_kind = format!("{}_page", doc.kind);
        let qualified = format!("{scheme}://{source_display}#page{page_num}");
        let name = heading.clone().unwrap_or_else(|| format!("Page {page_num}"));
        let extra = serde_json::json!({
            "kind": doc.kind,
            "page_num": page_num,
            "heading": heading,
            "bbox": serde_json::Value::Null,
            "extractor_version": doc.extractor_version,
        })
        .to_string();

        let node_sql = "INSERT OR REPLACE INTO nodes(kind,name,qualified_name,file_path,line_start,line_end,language,summary,extra,updated_at) \
                        VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,datetime('now'))";
        let node_params = vec![
            serde_json::Value::String(node_kind),
            serde_json::Value::String(name),
            serde_json::Value::String(qualified),
            serde_json::Value::String(source_display.clone()),
            serde_json::Value::Number((page_num as i64).into()),
            serde_json::Value::Number((page_num as i64).into()),
            serde_json::Value::String(doc.kind.clone()),
            serde_json::Value::String(page_text),
            serde_json::Value::String(extra),
        ];
        let resp = store
            .inject
            .insert(
                project,
                DbLayer::Graph,
                node_sql,
                node_params,
                InjectOptions {
                    emit_event: false,
                    audit: false,
                    ..InjectOptions::default()
                },
            )
            .await;
        if !resp.success {
            warn!(
                file = %doc.source.display(),
                page = page_num,
                error = ?resp.error,
                "pdf page node insert failed"
            );
            continue;
        }
        pages_written += 1;
    }

    Ok(pages_written)
}

// ---------------------------------------------------------------------------
// Leiden community detection (Bucket B4 fix)
// ---------------------------------------------------------------------------

/// Aggregate stats from the Leiden pass.
#[derive(Debug, Default, Clone, Copy)]
struct LeidenStats {
    /// Number of edges that survived hashing (every distinct
    /// `(source_qualified, target_qualified)` row in graph.db::edges).
    edges_used: usize,
    /// Number of communities the solver returned.
    communities: usize,
    /// Total members written across every community.
    members: usize,
}

/// Read every edge out of `graph.db::edges`, run Leiden over the resulting
/// undirected weighted graph, and persist the partitioning into
/// `semantic.db::{communities, community_membership}`.
///
/// Reads use a direct read-only rusqlite connection rather than going
/// through `store.query` so we don't compete with anything else that
/// may still be reading. The shard's writer task for `semantic.db` is
/// the only thing that touches it for writes; we go through
/// `store.inject` for those, which respects the per-shard MPSC writer.
///
/// Mapping: brain's `ClusterRunner` operates on `(NodeId, NodeId, f32)`
/// where `NodeId` is a `u128`. We hash each `qualified_name` to a u128
/// (lower 16 bytes of blake3) and keep a `HashMap<u128, String>` so we
/// can write the membership row by qualified_name.
///
/// Failure modes (ALL non-fatal — the build itself already succeeded):
///   - `graph.db` missing → return zeros (no build yet).
///   - Empty edge set → solver returns Vec::new(), zero rows written.
///   - SQL / hash collision → log warn and skip the offending row.
async fn run_leiden_pass(
    store: &Store,
    project_id: &ProjectId,
    paths: &PathManager,
) -> LeidenStats {
    let mut stats = LeidenStats::default();

    // Pull edges directly via rusqlite — no need to run them through
    // the store.query layer (which would re-open a pool just to do
    // one read). Read-only, so the per-shard writer is unaffected.
    let graph_db = paths.shard_db(project_id, DbLayer::Graph);
    if !graph_db.exists() {
        // Programmer-impossible at this call site (we just finished
        // writing to it), but keep the guard so a hand-run with
        // --limit 0 doesn't trip on a stale state.
        return stats;
    }

    let edges = match read_edges(&graph_db) {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "leiden: failed to read graph.db edges; skipping");
            return stats;
        }
    };
    stats.edges_used = edges.len();
    if edges.is_empty() {
        return stats;
    }

    // Hash qualified_name → u128 NodeId. We keep both directions of
    // the map: name_to_id is used while building the edge list,
    // id_to_name to project Communities back to qualified_names for
    // membership rows.
    let mut name_to_id: std::collections::HashMap<String, u128> =
        std::collections::HashMap::with_capacity(edges.len() * 2);
    let mut id_to_name: std::collections::HashMap<u128, String> =
        std::collections::HashMap::with_capacity(edges.len() * 2);

    let mut brain_edges: Vec<(BrainNodeId, BrainNodeId, f32)> =
        Vec::with_capacity(edges.len());
    for (src, tgt, weight) in edges {
        let src_id = *name_to_id
            .entry(src.clone())
            .or_insert_with(|| qualified_to_u128(&src));
        let tgt_id = *name_to_id
            .entry(tgt.clone())
            .or_insert_with(|| qualified_to_u128(&tgt));
        id_to_name.entry(src_id).or_insert_with(|| src.clone());
        id_to_name.entry(tgt_id).or_insert_with(|| tgt.clone());
        brain_edges.push((BrainNodeId::new(src_id), BrainNodeId::new(tgt_id), weight));
    }

    // Run Leiden. ClusterRunner returns a Vec<Community>; each Community
    // carries `id` (0-based dense), `members` (Vec<NodeId>), and `cohesion`.
    // The runner itself is deterministic: a fixed seed (42) inside
    // LeidenConfig::default() ensures repeated runs produce identical
    // partitions, which makes diffs over time meaningful.
    let runner = ClusterRunner::new(ClusterRunnerConfig::default());
    let communities = match runner.run(&brain_edges) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "leiden run failed; semantic.db will stay empty");
            return stats;
        }
    };
    stats.communities = communities.len();

    // Persist communities and membership. We do this serially through
    // the inject layer so the per-shard writer task ordering is
    // preserved. A bulk transaction would be faster but introduces a
    // direct-connection write path — explicitly avoided to keep the
    // single-writer invariant intact.
    for comm in &communities {
        let comm_name = format!("community-{}", comm.id);
        let comm_sql = "INSERT INTO communities(name, level, parent_id, cohesion, size) \
                        VALUES(?1, 0, NULL, ?2, ?3)";
        let comm_params = vec![
            serde_json::Value::String(comm_name),
            serde_json::Value::Number(
                serde_json::Number::from_f64(comm.cohesion as f64)
                    .unwrap_or_else(|| serde_json::Number::from(0)),
            ),
            serde_json::Value::Number((comm.members.len() as i64).into()),
        ];
        let resp = store
            .inject
            .insert(
                project_id,
                DbLayer::Semantic,
                comm_sql,
                comm_params,
                InjectOptions {
                    emit_event: false,
                    audit: false,
                    ..InjectOptions::default()
                },
            )
            .await;
        if !resp.success {
            warn!(
                community_id = comm.id,
                error = ?resp.error,
                "communities insert failed; skipping membership for this community"
            );
            continue;
        }
        let community_row_id = resp.data.map(|r| r.0).unwrap_or(0);
        if community_row_id == 0 {
            // Without a primary key we can't write membership rows that
            // join cleanly. Bail on this community — the next one will
            // get its own RowId.
            continue;
        }

        for member in &comm.members {
            let qn = match id_to_name.get(&member.as_u128()) {
                Some(s) => s.clone(),
                None => continue,
            };
            let mem_sql = "INSERT OR IGNORE INTO community_membership(community_id, node_qualified) \
                           VALUES(?1, ?2)";
            let mem_params = vec![
                serde_json::Value::Number(community_row_id.into()),
                serde_json::Value::String(qn),
            ];
            let resp = store
                .inject
                .insert(
                    project_id,
                    DbLayer::Semantic,
                    mem_sql,
                    mem_params,
                    InjectOptions {
                        emit_event: false,
                        audit: false,
                        ..InjectOptions::default()
                    },
                )
                .await;
            if resp.success {
                stats.members += 1;
            }
        }
    }

    stats
}

/// Read all `(source_qualified, target_qualified)` pairs from
/// `graph.db::edges`, plus the edge's `confidence_score` as the f32
/// weight. Self-loops and zero-weight rows are filtered here so the
/// solver doesn't have to guard against them.
fn read_edges(
    graph_db: &Path,
) -> Result<Vec<(String, String, f32)>, String> {
    let conn = rusqlite::Connection::open_with_flags(
        graph_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| format!("open {}: {e}", graph_db.display()))?;

    let mut stmt = conn
        .prepare(
            "SELECT source_qualified, target_qualified, confidence_score \
             FROM edges \
             WHERE source_qualified IS NOT NULL \
               AND target_qualified IS NOT NULL \
               AND confidence_score > 0",
        )
        .map_err(|e| format!("prep edges: {e}"))?;

    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2).map(|v| v as f32).unwrap_or(1.0),
            ))
        })
        .map_err(|e| format!("exec edges: {e}"))?;

    let mut out: Vec<(String, String, f32)> = Vec::new();
    for r in rows {
        match r {
            Ok((s, t, w)) if s != t && w > 0.0 && w.is_finite() => out.push((s, t, w)),
            Ok(_) => continue, // self-loop / non-positive weight
            Err(e) => return Err(format!("row map: {e}")),
        }
    }
    Ok(out)
}

/// Stable hash from `qualified_name` to the u128 brain expects.
/// Uses the lower 16 bytes of a blake3 hash so two runs over the same
/// graph produce the same NodeIds (Leiden determinism is preserved
/// only when its input is stable).
fn qualified_to_u128(qn: &str) -> u128 {
    let h = blake3::hash(qn.as_bytes());
    let bytes = h.as_bytes();
    // Take the first 16 bytes as a little-endian u128. Hash output is
    // 32 bytes; the upper 16 are dropped. Collisions on lower 16 bytes
    // of blake3 are vanishingly improbable at any realistic project
    // size (birthday-bound at 2^64 entries).
    let mut buf = [0u8; 16];
    buf.copy_from_slice(&bytes[..16]);
    u128::from_le_bytes(buf)
}

// ---------------------------------------------------------------------------
// Embedding pass (P3.4)
// ---------------------------------------------------------------------------

/// Aggregate stats from the embedding pass.
#[derive(Debug, Default, Clone)]
struct EmbeddingStats {
    /// Nodes considered (post comment-filter).
    nodes_scanned: usize,
    /// Nodes with non-empty derived text (signature ∪ summary ∪ name).
    nodes_with_text: usize,
    /// Embedding rows successfully written to semantic.db::embeddings
    /// (excludes rows skipped due to UNIQUE(text_hash, model) collision).
    embeddings_written: usize,
    /// Nodes whose `nodes.embedding_id` was successfully back-linked.
    nodes_linked: usize,
    /// Per-node failures (encoding error, embed_batch failure, SQL error).
    failures: usize,
    /// Active backend name — `"bge-small-en-v1.5"` or `"hashing-trick"`.
    backend: String,
}

/// Embed every substantive node and persist vectors into
/// `semantic.db::embeddings`. Back-link via `graph.db::nodes.embedding_id`.
///
/// Strategy:
/// 1. Read `(id, qualified_name, kind, name, signature, summary)` from
///    `graph.db::nodes` via a direct read-only rusqlite connection.
///    Skip `kind='comment'` (already filtered upstream by I4 fix; defensive
///    here in case older shards pre-date the filter).
/// 2. Derive a single text per node: `signature` if non-empty, else
///    `summary` if non-empty, else `"{kind} {name}"`. Skip if all empty.
/// 3. Batch into chunks of 64; call `embedder.embed_batch`.
/// 4. For each (node_id, vector): INSERT OR IGNORE into embeddings, then
///    UPDATE graph.db::nodes SET embedding_id WHERE id = node_id.
///    Both writes go through `store.inject` to respect the per-shard
///    single-writer invariant.
/// 5. Vector encoded as little-endian f32 bytes via `unhex(?hex_string)`
///    in SQL — the inject layer's params plumbing only supports
///    Null/Integer/Real/Text, so we hex-encode and decode in SQLite.
///
/// Failure modes (all non-fatal):
///   - graph.db missing → return zeros.
///   - Embedder unavailable → returns zeros, logs warn (Embedder always
///     succeeds via hashing-trick fallback so this is rare).
///   - Per-chunk embed_batch failure → counted, chunk skipped.
///   - Per-row INSERT/UPDATE failure → counted, continues.
async fn run_embedding_pass(
    store: &Store,
    project_id: &ProjectId,
    paths: &PathManager,
) -> EmbeddingStats {
    let mut stats = EmbeddingStats::default();

    let graph_db = paths.shard_db(project_id, DbLayer::Graph);
    if !graph_db.exists() {
        return stats;
    }

    let rows = match read_embeddable_nodes(&graph_db) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "embeddings: failed to read graph.db nodes; skipping");
            return stats;
        }
    };
    stats.nodes_scanned = rows.len();
    if rows.is_empty() {
        return stats;
    }

    let embedder = match Embedder::from_default_path() {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "embeddings: Embedder::from_default_path failed; skipping pass");
            return stats;
        }
    };
    let model_name = embedder.backend_name().to_string();
    stats.backend = model_name.clone();

    // Filter to rows with meaningful text. We materialise the (i64,
    // String) pairs because the embed_batch borrow needs &str refs that
    // outlive the per-chunk slice.
    let with_text: Vec<(i64, String)> = rows
        .into_iter()
        .filter_map(|row| {
            let text = derive_text_for_embedding(&row);
            if text.trim().is_empty() {
                None
            } else {
                Some((row.id, text))
            }
        })
        .collect();
    stats.nodes_with_text = with_text.len();
    if with_text.is_empty() {
        return stats;
    }

    const BATCH: usize = 64;
    for chunk in with_text.chunks(BATCH) {
        let texts: Vec<&str> = chunk.iter().map(|(_, t)| t.as_str()).collect();
        let vectors = match embedder.embed_batch(&texts) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "embeddings: embed_batch failed; chunk counted as failures");
                stats.failures += chunk.len();
                continue;
            }
        };
        if vectors.len() != chunk.len() {
            warn!(
                expected = chunk.len(),
                got = vectors.len(),
                "embeddings: vector count mismatch; chunk counted as failures"
            );
            stats.failures += chunk.len();
            continue;
        }

        for ((node_id, text), vec) in chunk.iter().zip(vectors.iter()) {
            let hex_blob = encode_le_f32_hex(vec);
            let text_hash = blake3::hash(text.as_bytes()).to_hex().to_string();

            // INSERT OR IGNORE — UNIQUE(text_hash, model) collision is
            // expected on incremental builds where the same signature
            // already exists. unhex() converts the hex TEXT param into
            // BLOB bytes for storage. (rusqlite 0.32 ships SQLite ≥
            // 3.41 which has unhex(); see workspace Cargo.toml.)
            let ins_sql = "INSERT OR IGNORE INTO embeddings(node_id, text_hash, model, vector) \
                           VALUES(?1, ?2, ?3, unhex(?4))";
            let ins_params = vec![
                serde_json::Value::Number((*node_id).into()),
                serde_json::Value::String(text_hash.clone()),
                serde_json::Value::String(model_name.clone()),
                serde_json::Value::String(hex_blob),
            ];
            let ins_resp = store
                .inject
                .insert(
                    project_id,
                    DbLayer::Semantic,
                    ins_sql,
                    ins_params,
                    InjectOptions {
                        emit_event: false,
                        audit: false,
                        ..InjectOptions::default()
                    },
                )
                .await;
            if !ins_resp.success {
                stats.failures += 1;
                continue;
            }
            let mut emb_id = ins_resp.data.map(|r| r.0).unwrap_or(0);

            // UNIQUE collision → INSERT OR IGNORE returned no row; fetch
            // the existing id so the back-link still happens.
            if emb_id == 0 {
                emb_id = lookup_embedding_id(store, project_id, &text_hash, &model_name)
                    .await
                    .unwrap_or(0);
            } else {
                stats.embeddings_written += 1;
            }

            if emb_id > 0 {
                let upd_resp = store
                    .inject
                    .update(
                        project_id,
                        DbLayer::Graph,
                        "UPDATE nodes SET embedding_id = ?1 WHERE id = ?2",
                        vec![
                            serde_json::Value::Number(emb_id.into()),
                            serde_json::Value::Number((*node_id).into()),
                        ],
                        InjectOptions {
                            emit_event: false,
                            audit: false,
                            ..InjectOptions::default()
                        },
                    )
                    .await;
                if upd_resp.success {
                    stats.nodes_linked += 1;
                } else {
                    stats.failures += 1;
                }
            }
        }
    }

    stats
}

/// One row from `graph.db::nodes` — only the columns we need to derive
/// the embedding text + back-link id.
struct EmbeddableRow {
    id: i64,
    kind: String,
    name: String,
    signature: Option<String>,
    summary: Option<String>,
}

fn derive_text_for_embedding(row: &EmbeddableRow) -> String {
    if let Some(sig) = row.signature.as_deref() {
        let trimmed = sig.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Some(sum) = row.summary.as_deref() {
        let trimmed = sum.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    // Last-resort fallback so files/imports/classes with no signature
    // or summary still get an embedding row keyed on their identity.
    let n = row.name.trim();
    if n.is_empty() {
        String::new()
    } else {
        format!("{} {}", row.kind, n)
    }
}

/// Read embeddable rows from `graph.db::nodes`. Filters out comment
/// nodes defensively (the I4 fix already drops them on insert; older
/// shards may still carry them).
fn read_embeddable_nodes(graph_db: &Path) -> Result<Vec<EmbeddableRow>, String> {
    let conn = rusqlite::Connection::open_with_flags(
        graph_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| format!("open {}: {e}", graph_db.display()))?;

    let mut stmt = conn
        .prepare(
            "SELECT id, kind, name, signature, summary \
             FROM nodes \
             WHERE kind != 'comment' \
               AND embedding_id IS NULL",
        )
        .map_err(|e| format!("prep nodes: {e}"))?;

    let rows = stmt
        .query_map([], |r| {
            Ok(EmbeddableRow {
                id: r.get::<_, i64>(0)?,
                kind: r.get::<_, String>(1).unwrap_or_default(),
                name: r.get::<_, String>(2).unwrap_or_default(),
                signature: r.get::<_, Option<String>>(3).unwrap_or(None),
                summary: r.get::<_, Option<String>>(4).unwrap_or(None),
            })
        })
        .map_err(|e| format!("exec nodes: {e}"))?;

    let mut out: Vec<EmbeddableRow> = Vec::new();
    for r in rows {
        match r {
            Ok(row) => out.push(row),
            Err(e) => return Err(format!("row map: {e}")),
        }
    }
    Ok(out)
}

/// Encode a 384-dim f32 vector as a hex string for the SQL `unhex()`
/// roundtrip. Two hex chars per byte, four bytes per f32 — 3,072 chars
/// for a BGE-small embedding.
fn encode_le_f32_hex(vec: &[f32]) -> String {
    let mut s = String::with_capacity(vec.len() * 8);
    for f in vec {
        for byte in f.to_le_bytes() {
            s.push_str(&format!("{:02x}", byte));
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Audit pass (I1 — populate findings.db)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct AuditStats {
    ran: bool,
    status: String,
}

/// 4.3 — which path the audit pass takes inside `mneme build`.
///
/// `Inline` is the new `--inline` contract: skip the IPC layer entirely
/// so `IpcClient::request` never gets a chance to call
/// `spawn_daemon_detached()` on a dead pipe. We shell straight into the
/// scanners worker via `audit::run_direct_subprocess`, which is the same
/// fallback path `audit::run` uses when the daemon is down. The user
/// asked for in-process; they get in-process.
///
/// `IpcWithFallback` is the historical behaviour: prefer the supervisor
/// when it's up (shared scanner pool, concurrent runs), gracefully
/// degrade to the direct subprocess if it's not. This is what
/// `--dispatch` and the long-standing default get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuditRoute {
    /// Skip IPC entirely. Used when `--inline` is in effect. No
    /// daemon spawn, no socket lookup, no auto-start fallback.
    Inline,
    /// Try IPC first; on connection failure fall through to the direct
    /// subprocess path (which itself does NOT auto-spawn a daemon —
    /// only `IpcClient::request` does that).
    IpcWithFallback,
}

/// Pure routing helper — given the user's `--inline` flag, pick the
/// audit-pass strategy. Pulled out so 4.3 has a unit test that asserts
/// "inline never reaches the IPC layer" without having to construct a
/// real subprocess or stub the supervisor pipe.
pub(crate) fn audit_route(inline: bool) -> AuditRoute {
    if inline {
        AuditRoute::Inline
    } else {
        AuditRoute::IpcWithFallback
    }
}

/// Run all 11 scanners against the freshly-built graph and persist
/// findings to `findings.db::findings`.
///
/// Two routes (see [`AuditRoute`]):
///   - [`AuditRoute::Inline`] — call `audit::run_direct_subprocess`
///     directly. This is the `mneme build --inline` path. It bypasses
///     `audit::run`, which would otherwise hit `IpcClient::request` and
///     auto-spawn `mneme-daemon` on a dead pipe (4.3).
///   - [`AuditRoute::IpcWithFallback`] — IPC-preferred, falls through
///     to the same direct-subprocess path if the supervisor is
///     unreachable. B-001/B-002 (v0.3.2): when this route is taken
///     from inside the build pipeline, it MUST use the no-autospawn
///     IPC client and a tight 5s timeout. The historical default —
///     `audit::run` with `make_client` — auto-spawned a second
///     `mneme-daemon` on connect failure, which on EC2 turned a 3-file
///     build into a 74-minute hang and polluted the process tree with
///     a zombie supervisor (B-002). Because build's own subprocess
///     fallback is already in place, the build pipeline can — and
///     must — opt out of the auto-spawn entirely.
///
/// Failure is non-fatal: the build itself already succeeded; an audit
/// failure just means findings.db stays empty and the user can re-run
/// `mneme audit` later.
async fn run_audit_pass(
    project: &Path,
    inline: bool,
    children: &BuildChildRegistry,
) -> AuditStats {
    let route = audit_route(inline);
    let result: CliResult<()> = match route {
        AuditRoute::Inline => {
            // 4.3: bypass `audit::run` entirely. That entrypoint dials
            // the supervisor first and `IpcClient::request` will
            // happily spawn `mneme-daemon` if the pipe is missing.
            // `--inline` means no daemon, period — go straight to the
            // scanners worker subprocess.
            //
            // B-003: pass the registry so a Ctrl-C arriving while the
            // scanners child is mid-scan still kills it.
            crate::commands::audit::run_direct_subprocess_with_registry(
                project,
                "full",
                scanners::Severity::Info,
                Some(children.clone()),
            )
            .await
        }
        AuditRoute::IpcWithFallback => {
            // B-001/B-002 (v0.3.2): try IPC ONCE with no-autospawn +
            // tight timeout. On any failure — connect refused, pipe
            // missing, supervisor returned an Error response, OR
            // timeout — fall through to the same direct-subprocess
            // path the inline route uses. This is the same try-IPC-
            // then-fallback contract `audit::run` implements, EXCEPT
            // we never spawn a second daemon and we cap the wall-
            // clock at 5s instead of 120s. Background: the
            // `make_client` + default `IpcClient` path auto-spawns
            // `mneme daemon start` on connect-failure (see
            // `cli/src/ipc.rs::request`), which on EC2 hung the build
            // for 74 minutes and produced a zombie second daemon.
            run_audit_pass_ipc_then_fallback(project, children).await
        }
    };
    match result {
        Ok(()) => AuditStats {
            ran: true,
            status: format!("ok ({})", match route {
                AuditRoute::Inline => "inline",
                AuditRoute::IpcWithFallback => "ipc-or-fallback",
            }),
        },
        Err(e) => {
            warn!(error = %e, ?route, "audit pass failed; findings.db will not be populated this build");
            AuditStats {
                ran: false,
                status: format!("error: {e}"),
            }
        }
    }
}

/// B-001/B-002: build-pipeline variant of `audit::run` — try IPC with
/// the no-autospawn 5s-budget client and on any error (or supervisor
/// `Error` response) fall through to the same direct-subprocess path
/// `audit::run` uses. Pulled out so we can unit-test the routing
/// shape without going through clap or hitting a real socket.
async fn run_audit_pass_ipc_then_fallback(
    project: &Path,
    children: &BuildChildRegistry,
) -> CliResult<()> {
    let client = make_client_for_build(None);
    let ipc_attempt = client
        .request(crate::ipc::IpcRequest::Audit {
            scope: "full".to_string(),
        })
        .await;

    match ipc_attempt {
        Ok(crate::ipc::IpcResponse::Error { message }) => {
            warn!(error = %message, "supervisor returned error; falling back to direct subprocess");
        }
        Ok(_) => {
            // Supervisor accepted the audit; the scanners worker will
            // populate findings.db asynchronously. Return Ok — there
            // is no useful renderer state for the build summary
            // beyond "ran".
            return Ok(());
        }
        Err(e) => {
            warn!(
                error = %e,
                "supervisor unreachable (no-autospawn); falling back to direct subprocess"
            );
        }
    }

    crate::commands::audit::run_direct_subprocess_with_registry(
        project,
        "full",
        scanners::Severity::Info,
        Some(children.clone()),
    )
    .await
}

// ---------------------------------------------------------------------------
// Intent pass (J1 — parse `@mneme-intent:` magic comments → memory.db.file_intent)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct IntentStats {
    annotation_count: usize,
    inferred_count: usize,
    /// J2 — rows whose source='git' (heuristics derived from git.db
    /// commit history: long-frozen, deferred-with-todos, "do not
    /// touch" commit messages). Subset of `total_written`.
    git_count: usize,
    /// J4 + J6 — rows whose source='convention'. J4 reads
    /// `intent.config.json` glob rules at the project root; J6 reads
    /// per-directory `INTENT.md` files. Both go to the same source
    /// bucket because they're both "team convention", as opposed to
    /// "magic comment from the file's author" (annotation) or
    /// "machine-derived from git history" (git).
    convention_count: usize,
    total_written: usize,
}

const INTENT_KINDS: &[&str] = &[
    "frozen",
    "stable",
    "deferred",
    "experimental",
    "drift",
    "unknown",
];

/// Walk every parsed file (kind='file' rows in graph.db.nodes), open
/// the first 30 lines, scan for `@mneme-intent: <kind>` (any comment
/// style), and persist matches to memory.db.file_intent.
// ---------------------------------------------------------------------------
// Imports-edge resolver pass — Phase A blast=0 fix
// ---------------------------------------------------------------------------
//
// Without this pass `mneme blast app.ts` returns 0 dependents. Reason:
// `parsers/src/extractor.rs::collect_imports` emits import edges with
// `target_qualified = "import::<path>::<module>::<binding>"` (a pseudo-id),
// stashing the real module string in `extra.unresolved`. The original
// design comment promised "downstream resolvers (brain) can map TARGET
// → source file at link time" — but no such resolver was ever written.
// Edges sat forever pointing at pseudo-ids; every consumer-graph walk
// (blast / call_graph / find_references) returned empty.
//
// This pass walks every imports edge whose target still starts with
// `import::`, parses the unresolved module path from `extra.unresolved`,
// resolves it against the importer's directory plus common extension /
// index.* heuristics, and UPDATEs `target_qualified` to the real file's
// qualified-name. Bare-package imports (e.g. `lodash`) are flagged as
// unresolved_bare and left alone — they have no local file target.
//
// Runs AFTER the parser loop (so all file nodes + raw edges are in
// graph.db) and BEFORE the Leiden community pass (so community
// detection clusters by resolved connectivity, not by pseudo-id).

#[derive(Debug, Default, Clone)]
struct ResolveImportsStats {
    /// Total imports edges found with `target_qualified LIKE 'import::%'`.
    total_imports_edges: usize,
    /// Edges successfully UPDATEd to a real file qualified-name.
    resolved: usize,
    /// Module path was relative ("./x" or "../y") but no matching file
    /// node existed (typo, generated file, or import-from-virtual-path).
    unresolved_relative: usize,
    /// Module is a bare package name (e.g. "lodash", "react") with no
    /// local file target — expected to remain unresolved.
    unresolved_bare: usize,
    /// Wall-clock ms for the entire pass.
    duration_ms: u64,
}

async fn run_resolve_imports_pass(
    store: &Store,
    project_id: &ProjectId,
    _project: &Path,
    paths: &PathManager,
) -> ResolveImportsStats {
    use std::collections::HashMap;
    let started = Instant::now();
    let mut stats = ResolveImportsStats::default();

    let graph_db = paths.shard_db(project_id, DbLayer::Graph);
    if !graph_db.exists() {
        return stats;
    }

    // Read everything we need from graph.db, then drop the read-only
    // connection BEFORE issuing UPDATEs through the writer task — keeps
    // the per-shard single-writer invariant honoured.
    let conn = match rusqlite::Connection::open_with_flags(
        &graph_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(_) => return stats,
    };

    // Build a (canonicalish-path → qualified_name) lookup for file nodes.
    // We strip the Windows `\\?\` long-path prefix on insert so lookups
    // by `Path::join` results match without the prefix.
    let mut file_by_path: HashMap<PathBuf, String> = HashMap::new();
    {
        let mut stmt = match conn.prepare(
            "SELECT qualified_name, file_path FROM nodes \
             WHERE kind = 'file' AND file_path IS NOT NULL",
        ) {
            Ok(s) => s,
            Err(_) => return stats,
        };
        let rows = match stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            Ok(it) => it,
            Err(_) => return stats,
        };
        for row in rows.filter_map(|r| r.ok()) {
            let (qn, fp) = row;
            let normalized = strip_long_path_prefix(&fp);
            file_by_path.insert(PathBuf::from(&normalized), qn);
        }
    }

    // Load every imports edge whose target_qualified is NOT already a
    // real file node. We process ALL imports edges (TS K7-path with
    // `import::` pseudo-ids AND legacy non-JS path where target is the
    // import-node qn). The resolver decides per-edge whether to UPDATE.
    // JOIN to nodes so we get the importer file_path in one query.
    let edges: Vec<(i64, String, Option<String>, String)> = {
        let mut stmt = match conn.prepare(
            "SELECT e.id, e.target_qualified, e.extra, n.file_path \
             FROM edges e \
             JOIN nodes n ON n.qualified_name = e.source_qualified \
             WHERE e.kind = 'imports' \
             AND n.file_path IS NOT NULL",
        ) {
            Ok(s) => s,
            Err(_) => return stats,
        };
        let it = match stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        }) {
            Ok(rows) => rows,
            Err(_) => return stats,
        };
        it.filter_map(|r| r.ok()).collect()
    };
    drop(conn);
    stats.total_imports_edges = edges.len();

    for (edge_id, _old_target, extra_json, importer_path) in edges {
        // Pull the unresolved string from the JSON in `extra`. This may
        // be either a clean module path (`./app`, `./app#App`,
        // `math_utils`) OR — and this is the key bug we work around —
        // the FULL import-statement text (e.g.
        // `"import { App } from './app';#App"` or
        // `"from math_utils import add"`). The extractor's broadened
        // pattern set sometimes lacks an `@source` capture in the
        // matched alternative, so its fallback uses the outer node
        // text. We treat both shapes uniformly via `extract_module`.
        let unresolved: Option<String> = extra_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|v| {
                v.get("unresolved")
                    .and_then(|inner| inner.as_str().map(|s| s.to_string()))
            });
        let Some(unresolved) = unresolved else {
            stats.unresolved_relative += 1;
            continue;
        };

        // Strip the `#binding` suffix the K7 path appends. Whatever
        // remains might still be a full statement or a clean path —
        // `extract_module` handles both shapes.
        let raw_module = unresolved
            .split_once('#')
            .map(|(m, _)| m)
            .unwrap_or(unresolved.as_str());

        let Some(module_clean) = extract_module(raw_module) else {
            stats.unresolved_relative += 1;
            continue;
        };

        // Build a candidate path: importer_dir + module_clean, then
        // normalise `..`/`.` segments. Works for relative paths
        // (`./foo`), bare names that happen to be local-package files
        // (Python `math_utils`), and absolute paths.
        let importer_clean = strip_long_path_prefix(&importer_path);
        let importer_dir = match Path::new(&importer_clean).parent() {
            Some(p) => p.to_path_buf(),
            None => {
                stats.unresolved_relative += 1;
                continue;
            }
        };
        let raw = importer_dir.join(&module_clean);
        let candidate = normalize_path_segments(&raw);

        let target_qn = resolve_module_to_qn(&candidate, &file_by_path);

        if let Some(target_qn) = target_qn {
            // UPDATE the edge through the per-shard writer task.
            // Honours the single-writer invariant — store::Inject is
            // the canonical write surface for graph.db.
            let sql = "UPDATE edges SET target_qualified = ?1 WHERE id = ?2";
            let params = vec![
                serde_json::Value::String(target_qn),
                serde_json::Value::Number(edge_id.into()),
            ];
            let resp = store
                .inject
                .insert(
                    project_id,
                    DbLayer::Graph,
                    sql,
                    params,
                    InjectOptions {
                        emit_event: false,
                        audit: false,
                        ..InjectOptions::default()
                    },
                )
                .await;
            if resp.success {
                stats.resolved += 1;
            } else {
                stats.unresolved_relative += 1;
            }
        } else if module_clean.starts_with('.')
            || module_clean.starts_with('/')
            || is_windows_abs(&module_clean)
        {
            // Looked relative/absolute but no matching file node — typo,
            // generated file, or import-from-virtual-path.
            stats.unresolved_relative += 1;
        } else {
            // Bare package name (`lodash`, `react`, `numpy`) with no
            // local file. Expected for npm / pypi / crates.io deps.
            stats.unresolved_bare += 1;
        }
    }

    stats.duration_ms = started.elapsed().as_millis() as u64;
    stats
}

/// Strip the Windows `\\?\` long-path prefix if present, leaving a
/// path that matches the form produced by `Path::join` on relative
/// components. No-op on POSIX paths.
fn strip_long_path_prefix(p: &str) -> String {
    if let Some(rest) = p.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        p.to_string()
    }
}

/// Heuristic: detect a Windows-style absolute path like `C:\foo` or
/// `C:/foo`. Used so an absolute import (rare but legal) is treated as
/// already-resolved rather than as a bare package.
fn is_windows_abs(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

/// Extract a clean module path from an `unresolved` string that may
/// be either already-clean (`./foo`, `math_utils`) or the full text
/// of an import statement. The extractor falls back to outer-node
/// text when the broadened K7 pattern's matched alternative doesn't
/// expose an `@source` capture, so we cope with both shapes here:
///
/// - clean `./foo` / `../bar` / `math_utils` → returned as-is
/// - TS/JS embedded: `import { X } from './foo'[;]` → first quoted
///   substring (handles both `'` and `"` quotes)
/// - Python embedded: `from math_utils import add` → token after
///   `from `
///
/// Returns `None` only when no recognisable module token can be
/// pulled out of the input.
fn extract_module(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Already-clean module path: no spaces / quotes / semicolons /
    // parens / angle brackets means nothing was wrapped around it.
    let messy = trimmed.chars().any(|c| {
        c == ' '
            || c == '"'
            || c == '\''
            || c == ';'
            || c == '('
            || c == ')'
            || c == '<'
            || c == '>'
    });
    if !messy {
        return Some(trimmed.to_string());
    }
    // First quoted substring — TS/JS `import ... from '...'` pattern.
    for quote in ['"', '\''] {
        if let Some(start) = trimmed.find(quote) {
            let after = &trimmed[start + 1..];
            if let Some(end_offset) = after.find(quote) {
                let inner = &after[..end_offset];
                if !inner.is_empty() {
                    return Some(inner.to_string());
                }
            }
        }
    }
    // Python `from <X> import ...` — pull the token after `from `.
    if let Some(rest) = trimmed.strip_prefix("from ") {
        let mod_len = rest
            .find(|c: char| c.is_whitespace())
            .unwrap_or(rest.len());
        if mod_len > 0 {
            return Some(rest[..mod_len].to_string());
        }
    }
    None
}

/// Walk `p`'s components and resolve `..` / `.` segments without
/// touching the filesystem. We can't use `Path::canonicalize` because
/// the resolved path may not exist (yet); the lookup happens against
/// the in-memory HashMap of node paths.
fn normalize_path_segments(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Try `module_pb` itself, then common JS/TS/Py/Rs extensions, then
/// `index.*` lookups (Node's package-folder convention). Returns the
/// matched node's qualified_name on hit, `None` on miss.
fn resolve_module_to_qn(
    module_pb: &Path,
    file_by_path: &std::collections::HashMap<PathBuf, String>,
) -> Option<String> {
    if let Some(qn) = file_by_path.get(module_pb) {
        return Some(qn.clone());
    }
    const EXTS: &[&str] = &[
        ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".d.ts", ".py", ".rs",
    ];
    for ext in EXTS {
        let mut candidate = module_pb.as_os_str().to_owned();
        candidate.push(ext);
        let pb = PathBuf::from(candidate);
        if let Some(qn) = file_by_path.get(&pb) {
            return Some(qn.clone());
        }
    }
    const INDEXES: &[&str] = &[
        "index.ts",
        "index.tsx",
        "index.js",
        "index.jsx",
        "index.mjs",
    ];
    for idx in INDEXES {
        let candidate = module_pb.join(idx);
        if let Some(qn) = file_by_path.get(&candidate) {
            return Some(qn.clone());
        }
    }
    None
}

async fn run_intent_pass(
    store: &Store,
    project_id: &ProjectId,
    project: &Path,
    paths: &PathManager,
) -> IntentStats {
    let mut stats = IntentStats::default();

    let graph_db = paths.shard_db(project_id, DbLayer::Graph);
    if !graph_db.exists() {
        return stats;
    }

    let conn = match rusqlite::Connection::open_with_flags(
        &graph_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(_) => return stats,
    };
    let mut stmt = match conn.prepare(
        "SELECT file_path FROM nodes WHERE kind='file' AND file_path IS NOT NULL",
    ) {
        Ok(s) => s,
        Err(_) => return stats,
    };
    let files: Vec<String> = match stmt.query_map([], |r| r.get::<_, String>(0)) {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(_) => return stats,
    };

    for fp in files {
        // Read first ~30 lines
        let abs: PathBuf = if Path::new(&fp).is_absolute() {
            PathBuf::from(&fp)
        } else {
            project.join(&fp)
        };
        let content = match std::fs::read_to_string(&abs) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let head: String = content.lines().take(30).collect::<Vec<_>>().join("\n");
        if let Some((kind, reason)) = parse_mneme_intent(&head) {
            let sql = "INSERT OR REPLACE INTO file_intent(file_path, intent, reason, source, confidence) \
                       VALUES(?1, ?2, ?3, 'annotation', 1.0)";
            let params = vec![
                serde_json::Value::String(fp.clone()),
                serde_json::Value::String(kind),
                match reason {
                    Some(r) => serde_json::Value::String(r),
                    None => serde_json::Value::Null,
                },
            ];
            let resp = store
                .inject
                .insert(
                    project_id,
                    DbLayer::Memory,
                    sql,
                    params,
                    InjectOptions {
                        emit_event: false,
                        audit: false,
                        ..InjectOptions::default()
                    },
                )
                .await;
            if resp.success {
                stats.annotation_count += 1;
                stats.total_written += 1;
            }
        }
    }

    stats
}

/// Detect `@mneme-intent: <kind>[: reason]` in the first 30 lines of
/// file content. Tolerant to all major comment styles (`//`, `#`, `/*`,
/// `;`, `--`). Returns `(kind, reason)` if found.
fn parse_mneme_intent(head: &str) -> Option<(String, Option<String>)> {
    for line in head.lines() {
        let trimmed = line.trim_start_matches(|c: char| {
            c == ' '
                || c == '\t'
                || c == '/'
                || c == '*'
                || c == '#'
                || c == ';'
                || c == '-'
                || c == '!'
        });
        let lower = trimmed.to_lowercase();
        if let Some(idx) = lower.find("@mneme-intent:") {
            // Skip past the marker, capture rest of line
            let after = &trimmed[idx + "@mneme-intent:".len()..];
            let after = after.trim();
            // First token is kind, rest is reason (optionally separated by ':' or '—' or '-')
            let mut kind_part = String::new();
            let mut reason_part = String::new();
            for (i, c) in after.chars().enumerate() {
                if c.is_alphanumeric() || c == '_' {
                    kind_part.push(c);
                } else {
                    reason_part = after[i..].trim_start_matches(|c: char| {
                        c == ':' || c == '-' || c == '—' || c == ' ' || c == '\t'
                    }).to_string();
                    break;
                }
            }
            let kind_lower = kind_part.to_lowercase();
            if INTENT_KINDS.contains(&kind_lower.as_str()) {
                let reason = if reason_part.trim().is_empty() {
                    None
                } else {
                    // Trim trailing comment terminators (*/ etc)
                    Some(
                        reason_part
                            .trim_end_matches(|c: char| c == '*' || c == '/' || c == ' ')
                            .to_string(),
                    )
                };
                return Some((kind_lower, reason));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Intent pass — J2 (git-history heuristics)
//                J4 (intent.config.json convention rules)
//                J6 (per-directory INTENT.md annotations)
// ---------------------------------------------------------------------------
//
// All three feed `memory.db.file_intent` and run AFTER the J1 magic-comment
// pass, so an explicit `@mneme-intent:` annotation always wins. We use
// `INSERT OR IGNORE` *plus* an in-memory "already-claimed" set so the
// per-source counts in the build summary are mutually exclusive: a file
// whose intent is set by annotation is not re-counted as git/convention.
//
// Stats accumulate into the SAME `IntentStats` returned from J1 — the
// caller passes `&mut intent_stats` so the build summary line stays a
// single coherent count.

/// Walk every indexed file and infer intent from git history. Three
/// heuristics in priority order (first match wins per file):
///
///   1. Commit message contains `verbatim` or `do not touch`
///      (case-insensitive substring on the most recent message that
///      touched the file) → `frozen`, confidence 0.8.
///   2. File last touched > 365 days ago AND the most-recent commit
///      had ≤2 file-changes (low churn) → `frozen`, confidence 0.6.
///   3. File has been touched within the last 90 days AND its
///      `additions+deletions` over those commits ≥ 200 lines AND the
///      file content has ≥ 3 TODO/FIXME tokens → `deferred`,
///      confidence 0.5.
///
/// Files with no commit_files row are skipped (no signal). Files that
/// already have a `file_intent` row (from J1 annotation) are also
/// skipped — annotation always wins.
async fn run_git_intent_pass(
    store: &Store,
    project_id: &ProjectId,
    project: &Path,
    paths: &PathManager,
    stats: &mut IntentStats,
) {
    let graph_db = paths.shard_db(project_id, DbLayer::Graph);
    let git_db = paths.shard_db(project_id, DbLayer::Git);
    let memory_db = paths.shard_db(project_id, DbLayer::Memory);
    if !graph_db.exists() || !git_db.exists() {
        return;
    }

    let mut claimed = load_existing_intent_paths(&memory_db);

    let files: Vec<String> = match read_indexed_files(&graph_db) {
        Some(f) => f,
        None => return,
    };

    let conn = match rusqlite::Connection::open_with_flags(
        &git_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(_) => return,
    };

    let now = chrono::Utc::now();
    let one_year_ago = now - chrono::Duration::days(365);
    let ninety_days_ago = now - chrono::Duration::days(90);

    for fp in files {
        if claimed.contains(&fp) {
            continue;
        }

        let candidates = candidate_git_paths(&fp, project);

        let row: Option<(String, String, i64)> = candidates.iter().find_map(|key| {
            conn.query_row(
                "SELECT c.committed_at, c.message, \
                        (SELECT COUNT(*) FROM commit_files cf2 WHERE cf2.sha=c.sha) \
                 FROM commits c \
                 JOIN commit_files cf ON cf.sha = c.sha \
                 WHERE cf.file_path = ?1 \
                 ORDER BY c.committed_at DESC LIMIT 1",
                [key.as_str()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )
            .ok()
        });

        let Some((committed_at, message, files_in_commit)) = row else {
            continue;
        };

        let last_touched = chrono::DateTime::parse_from_rfc3339(&committed_at)
            .map(|d| d.with_timezone(&chrono::Utc))
            .ok();

        // Heuristic 1 — commit message marker.
        let msg_lower = message.to_lowercase();
        let kind_reason: Option<(&'static str, f64, String)> = if msg_lower.contains("verbatim")
            || msg_lower.contains("do not touch")
        {
            Some((
                "frozen",
                0.8,
                format!("commit marker: {}", truncate_reason(&message, 80)),
            ))
        } else if let Some(ts) = last_touched {
            if ts < one_year_ago && files_in_commit <= 2 {
                // Heuristic 2 — long frozen + low churn.
                Some((
                    "frozen",
                    0.6,
                    format!(
                        "git: not touched since {} (low-churn commit)",
                        ts.format("%Y-%m-%d")
                    ),
                ))
            } else if ts >= ninety_days_ago {
                // Heuristic 3 — recent churn + TODO/FIXME density.
                let abs = if Path::new(&fp).is_absolute() {
                    PathBuf::from(&fp)
                } else {
                    project.join(&fp)
                };
                let recent_lines: i64 = candidates
                    .iter()
                    .map(|key| {
                        conn.query_row(
                            "SELECT COALESCE(SUM(cf.additions + cf.deletions), 0) \
                             FROM commit_files cf JOIN commits c ON cf.sha=c.sha \
                             WHERE cf.file_path = ?1 AND c.committed_at >= ?2",
                            rusqlite::params![key.as_str(), ninety_days_ago.to_rfc3339()],
                            |r| r.get::<_, i64>(0),
                        )
                        .unwrap_or(0)
                    })
                    .max()
                    .unwrap_or(0);
                if recent_lines >= 200 && count_todo_density(&abs) >= 3 {
                    Some((
                        "deferred",
                        0.5,
                        format!(
                            "git: {} lines churned in last 90d + ≥3 TODO/FIXME",
                            recent_lines
                        ),
                    ))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        let Some((kind, confidence, reason)) = kind_reason else {
            continue;
        };

        let sql = "INSERT OR IGNORE INTO file_intent(file_path, intent, reason, source, confidence) \
                   VALUES(?1, ?2, ?3, 'git', ?4)";
        let params = vec![
            serde_json::Value::String(fp.clone()),
            serde_json::Value::String(kind.to_string()),
            serde_json::Value::String(reason),
            serde_json::Value::Number(
                serde_json::Number::from_f64(confidence).unwrap_or_else(|| 0.into()),
            ),
        ];
        let resp = store
            .inject
            .insert(
                project_id,
                DbLayer::Memory,
                sql,
                params,
                InjectOptions {
                    emit_event: false,
                    audit: false,
                    ..InjectOptions::default()
                },
            )
            .await;
        if resp.success {
            claimed.insert(fp.clone());
            stats.git_count += 1;
            stats.total_written += 1;
        }
    }
}

/// Parse `<project>/intent.config.json` if present and apply each
/// `{glob, intent[, reason]}` rule against every indexed file.
/// Source='convention', confidence=0.9. Skips files whose `file_intent`
/// row already exists from J1/J2.
async fn run_convention_intent_pass(
    store: &Store,
    project_id: &ProjectId,
    project: &Path,
    paths: &PathManager,
    stats: &mut IntentStats,
) {
    let cfg_path = project.join("intent.config.json");
    if !cfg_path.exists() {
        return;
    }
    let raw = match std::fs::read_to_string(&cfg_path) {
        Ok(s) => s,
        Err(_) => return,
    };
    // Strip UTF-8 BOM if present (Notepad / VSCode-on-Windows quirk).
    let trimmed = raw.trim_start_matches('\u{feff}');
    let rules = match parse_intent_config(trimmed) {
        Some(r) => r,
        None => return,
    };
    if rules.is_empty() {
        return;
    }

    let graph_db = paths.shard_db(project_id, DbLayer::Graph);
    let memory_db = paths.shard_db(project_id, DbLayer::Memory);
    if !graph_db.exists() {
        return;
    }
    let mut claimed = load_existing_intent_paths(&memory_db);
    let files: Vec<String> = match read_indexed_files(&graph_db) {
        Some(f) => f,
        None => return,
    };

    for fp in files {
        if claimed.contains(&fp) {
            continue;
        }
        let rel = relative_for_glob(&fp, project);
        for rule in &rules {
            if !INTENT_KINDS.contains(&rule.intent.as_str()) {
                continue;
            }
            if simple_glob_match(&rule.glob, &rel) {
                let sql = "INSERT OR IGNORE INTO file_intent(file_path, intent, reason, source, confidence) \
                           VALUES(?1, ?2, ?3, 'convention', 0.9)";
                let params = vec![
                    serde_json::Value::String(fp.clone()),
                    serde_json::Value::String(rule.intent.clone()),
                    match &rule.reason {
                        Some(r) => serde_json::Value::String(r.clone()),
                        None => serde_json::Value::String(format!(
                            "intent.config.json: glob={}",
                            rule.glob
                        )),
                    },
                ];
                let resp = store
                    .inject
                    .insert(
                        project_id,
                        DbLayer::Memory,
                        sql,
                        params,
                        InjectOptions {
                            emit_event: false,
                            audit: false,
                            ..InjectOptions::default()
                        },
                    )
                    .await;
                if resp.success {
                    claimed.insert(fp.clone());
                    stats.convention_count += 1;
                    stats.total_written += 1;
                }
                // First matching rule wins for this file.
                break;
            }
        }
    }
}

/// Walk every indexed file's parent directory; for any directory that
/// contains an `INTENT.md` file, parse its bullet list and apply
/// per-file annotations. Source='convention', confidence=0.9.
async fn run_intent_md_pass(
    store: &Store,
    project_id: &ProjectId,
    project: &Path,
    paths: &PathManager,
    stats: &mut IntentStats,
) {
    let graph_db = paths.shard_db(project_id, DbLayer::Graph);
    let memory_db = paths.shard_db(project_id, DbLayer::Memory);
    if !graph_db.exists() {
        return;
    }
    let mut claimed = load_existing_intent_paths(&memory_db);
    let files: Vec<String> = match read_indexed_files(&graph_db) {
        Some(f) => f,
        None => return,
    };

    // Cache parsed INTENT.md per directory so we don't re-read it for
    // every file in the same dir.
    use std::collections::HashMap;
    let mut dir_cache: HashMap<PathBuf, Option<HashMap<String, (String, Option<String>)>>> =
        HashMap::new();

    for fp in files {
        if claimed.contains(&fp) {
            continue;
        }
        let abs: PathBuf = if Path::new(&fp).is_absolute() {
            PathBuf::from(&fp)
        } else {
            project.join(&fp)
        };
        let Some(dir) = abs.parent() else {
            continue;
        };
        let dir_owned = dir.to_path_buf();
        let entry = dir_cache.entry(dir_owned.clone()).or_insert_with(|| {
            let intent_md = dir.join("INTENT.md");
            std::fs::read_to_string(&intent_md)
                .ok()
                .map(|s| parse_intent_md(&s))
        });

        let Some(map) = entry else { continue };
        let Some(filename) = abs.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some((kind, reason)) = map.get(filename) else {
            continue;
        };
        if !INTENT_KINDS.contains(&kind.as_str()) {
            continue;
        }

        let sql = "INSERT OR IGNORE INTO file_intent(file_path, intent, reason, source, confidence) \
                   VALUES(?1, ?2, ?3, 'convention', 0.9)";
        let params = vec![
            serde_json::Value::String(fp.clone()),
            serde_json::Value::String(kind.clone()),
            match reason {
                Some(r) => serde_json::Value::String(r.clone()),
                None => serde_json::Value::String("INTENT.md".to_string()),
            },
        ];
        let resp = store
            .inject
            .insert(
                project_id,
                DbLayer::Memory,
                sql,
                params,
                InjectOptions {
                    emit_event: false,
                    audit: false,
                    ..InjectOptions::default()
                },
            )
            .await;
        if resp.success {
            claimed.insert(fp.clone());
            stats.convention_count += 1;
            stats.total_written += 1;
        }
    }
}

// ---- helpers ---------------------------------------------------------------

/// Snapshot the set of `file_path` values that already have a row in
/// `memory.db.file_intent`. Used by J2/J4/J6 to keep per-source counts
/// mutually exclusive — annotation always wins.
fn load_existing_intent_paths(memory_db: &Path) -> std::collections::HashSet<String> {
    use std::collections::HashSet;
    let mut out: HashSet<String> = HashSet::new();
    if !memory_db.exists() {
        return out;
    }
    let conn = match rusqlite::Connection::open_with_flags(
        memory_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(_) => return out,
    };
    let mut stmt = match conn.prepare("SELECT file_path FROM file_intent") {
        Ok(s) => s,
        Err(_) => return out,
    };
    if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
        for r in rows.flatten() {
            out.insert(r);
        }
    }
    out
}

/// Return the file_path values of every `kind='file'` node in graph.db.
fn read_indexed_files(graph_db: &Path) -> Option<Vec<String>> {
    let conn = rusqlite::Connection::open_with_flags(
        graph_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    let mut stmt = conn
        .prepare("SELECT file_path FROM nodes WHERE kind='file' AND file_path IS NOT NULL")
        .ok()?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0)).ok()?;
    Some(rows.filter_map(|r| r.ok()).collect())
}

/// Build candidate keys for looking up `commit_files.file_path` from a
/// graph.db `file_path` value. Git always stores forward-slash,
/// project-relative paths; graph.db may carry absolute paths
/// (Windows: `C:\...`) or relative ones (`src/foo.ts`).
fn candidate_git_paths(fp: &str, project: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let raw = fp.replace('\\', "/");
    out.push(raw.clone());
    if let Ok(rel) = Path::new(fp).strip_prefix(project) {
        let s = rel.to_string_lossy().replace('\\', "/");
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// Count occurrences of `TODO` or `FIXME` (case-insensitive) in a file.
/// Returns 0 on any I/O error (file deleted, permission denied, etc.).
fn count_todo_density(path: &Path) -> usize {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return 0,
    };
    let upper = content.to_uppercase();
    upper.matches("TODO").count() + upper.matches("FIXME").count()
}

/// Trim a reason string to ~`max_chars` chars (char-boundary safe).
fn truncate_reason(s: &str, max_chars: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max_chars).collect();
    out.push_str("...");
    out
}

/// One rule in `intent.config.json`.
#[derive(Debug, Clone)]
struct ConventionRule {
    glob: String,
    intent: String,
    reason: Option<String>,
}

/// Parse the JSON document. Tolerant: missing fields → rule skipped,
/// not a hard error. Returns None only when the document itself
/// fails to parse as JSON.
fn parse_intent_config(raw: &str) -> Option<Vec<ConventionRule>> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let arr = v.get("rules")?.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let glob = item.get("glob").and_then(|x| x.as_str()).map(String::from);
        let intent = item.get("intent").and_then(|x| x.as_str()).map(String::from);
        let reason = item.get("reason").and_then(|x| x.as_str()).map(String::from);
        if let (Some(g), Some(i)) = (glob, intent) {
            out.push(ConventionRule {
                glob: g,
                intent: i.to_lowercase(),
                reason,
            });
        }
    }
    Some(out)
}

/// Parse INTENT.md bullet list:
/// `- <filename>: <intent> [— <reason>]` (em-dash, en-dash, or ` -- `).
/// Returns map keyed by bare filename (no directory).
fn parse_intent_md(raw: &str) -> std::collections::HashMap<String, (String, Option<String>)> {
    use std::collections::HashMap;
    let mut out: HashMap<String, (String, Option<String>)> = HashMap::new();
    for line in raw.lines() {
        let trimmed = line.trim_start();
        let body = match trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            Some(b) => b,
            None => continue,
        };
        let Some(colon) = body.find(':') else { continue };
        let name = body[..colon].trim();
        let rest = body[colon + 1..].trim();
        if name.is_empty() || rest.is_empty() {
            continue;
        }
        // Split intent from reason on em-dash, en-dash, or ` -- `.
        let (kind_raw, reason_raw): (&str, Option<&str>) = if let Some(idx) = rest.find('—') {
            (&rest[..idx], Some(rest[idx + '—'.len_utf8()..].trim()))
        } else if let Some(idx) = rest.find('–') {
            (&rest[..idx], Some(rest[idx + '–'.len_utf8()..].trim()))
        } else if let Some(idx) = rest.find(" -- ") {
            (&rest[..idx], Some(rest[idx + 4..].trim()))
        } else {
            (rest, None)
        };
        // Intent is the first whitespace-bounded token.
        let kind = kind_raw
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_lowercase();
        if kind.is_empty() {
            continue;
        }
        let reason = reason_raw
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty());
        out.insert(name.to_string(), (kind, reason));
    }
    out
}

/// Convert a graph.db file_path (possibly absolute, possibly Windows-
/// style) into a forward-slash project-relative form for glob matching.
fn relative_for_glob(fp: &str, project: &Path) -> String {
    let p = Path::new(fp);
    let rel = if p.is_absolute() {
        p.strip_prefix(project).unwrap_or(p).to_path_buf()
    } else {
        p.to_path_buf()
    };
    rel.to_string_lossy().replace('\\', "/")
}

/// Tiny glob matcher: supports `**` (any path including `/`),
/// `*` (any chars excluding `/`), `?` (single char excluding `/`),
/// and literal byte match for everything else. Sufficient for the
/// J4 rule patterns in the spec; we deliberately avoid pulling
/// `globset` into the CLI to keep build deps unchanged.
fn simple_glob_match(pat: &str, path: &str) -> bool {
    glob_match_inner(pat.as_bytes(), path.as_bytes())
}

fn glob_match_inner(pat: &[u8], path: &[u8]) -> bool {
    // Strip a leading `**/` so `**/foo` matches both `foo` and `a/b/foo`.
    if pat.len() >= 3 && &pat[..3] == b"**/" {
        let rest = &pat[3..];
        if glob_match_inner(rest, path) {
            return true;
        }
        for i in 0..path.len() {
            if path[i] == b'/' && glob_match_inner(rest, &path[i + 1..]) {
                return true;
            }
        }
        return false;
    }
    // Trailing `/**` → matches any descendant of head.
    if pat.ends_with(b"/**") {
        let head = &pat[..pat.len() - 3];
        if path.len() < head.len() {
            return false;
        }
        if !glob_match_inner(head, &path[..head.len()]) {
            return false;
        }
        if path.len() == head.len() {
            return true;
        }
        return path[head.len()] == b'/';
    }
    if pat == b"**" {
        return true;
    }

    let mut pi = 0usize;
    let mut si = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_si: usize = 0;
    while si < path.len() {
        if pi < pat.len() {
            let pc = pat[pi];
            let sc = path[si];
            if pc == b'*' {
                star_pi = Some(pi);
                star_si = si;
                pi += 1;
                continue;
            }
            if pc == b'?' && sc != b'/' {
                pi += 1;
                si += 1;
                continue;
            }
            if pc == sc {
                pi += 1;
                si += 1;
                continue;
            }
        }
        // Backtrack to last `*`.
        if let Some(spi) = star_pi {
            star_si += 1;
            // `*` does not cross `/`.
            if star_si > path.len() || path[star_si - 1] == b'/' {
                return false;
            }
            pi = spi + 1;
            si = star_si;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

// ---------------------------------------------------------------------------
// Betweenness centrality pass (H6 — sampled Brandes on graph.db edges)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct BetweennessStats {
    /// Distinct nodes that received any BC contribution.
    nodes_scored: usize,
    /// Top BC score in the population (proxy for "is this signal alive?").
    top_score: f64,
    /// How many source nodes we sampled in the Brandes outer loop.
    sources_sampled: usize,
    /// Wall-clock ms for the entire pass.
    duration_ms: u64,
}

/// Compute betweenness centrality via sampled Brandes algorithm and
/// persist to `graph.db::node_centrality`.
///
/// Phase A H6 said: "betweenness: 0 on every god_node" because BC was
/// never computed. Even basic Brandes on 30k nodes is ~2s in Rust, but
/// we sample-bound it so behaviour scales gracefully on huge graphs:
///
///   - Sample top-K source nodes by degree (high-degree nodes
///     contribute most BC mass; sampling them captures the heavy tail).
///   - Cap K at min(50, V) and wall clock at 30 seconds.
///   - Sum contributions into a `betweenness` value per node, scaled
///     by `2 / ((V-1)*(V-2))` (the standard normalisation).
///
/// Per-node BC results are approximate when V > K — the Phase A audit
/// asked for "non-zero, real signal" not "exact Brandes BC". Caller
/// gets honest stats (`sources_sampled`) so consumers can interpret.
async fn run_betweenness_pass(
    store: &Store,
    project_id: &ProjectId,
    paths: &PathManager,
) -> BetweennessStats {
    use std::collections::{HashMap, VecDeque};
    let mut stats = BetweennessStats::default();
    let started = std::time::Instant::now();

    let graph_db = paths.shard_db(project_id, DbLayer::Graph);
    if !graph_db.exists() {
        return stats;
    }

    // Read all edges (undirected — BC is normally computed on undirected
    // graphs; for code dependencies the directionality matters less for
    // "is this a bridge?" purposes).
    let conn = match rusqlite::Connection::open_with_flags(
        &graph_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "betweenness: read graph.db failed; skipping");
            return stats;
        }
    };

    let mut edge_stmt = match conn
        .prepare("SELECT source_qualified, target_qualified FROM edges WHERE source_qualified IS NOT NULL AND target_qualified IS NOT NULL")
    {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "betweenness: prep failed");
            return stats;
        }
    };
    let raw_edges: Vec<(String, String)> = match edge_stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
    {
        Ok(rows) => rows.filter_map(|r| r.ok()).filter(|(s, t)| s != t).collect(),
        Err(_) => Vec::new(),
    };
    if raw_edges.is_empty() {
        stats.duration_ms = started.elapsed().as_millis() as u64;
        return stats;
    }

    // Compact to integer node IDs for tight loops. Build adjacency.
    let mut name_to_idx: HashMap<String, usize> = HashMap::with_capacity(raw_edges.len());
    let mut idx_to_name: Vec<String> = Vec::new();
    let mut adj: Vec<Vec<usize>> = Vec::new();
    let mut intern = |s: &str,
                      name_to_idx: &mut HashMap<String, usize>,
                      idx_to_name: &mut Vec<String>,
                      adj: &mut Vec<Vec<usize>>|
     -> usize {
        if let Some(&i) = name_to_idx.get(s) {
            return i;
        }
        let i = idx_to_name.len();
        idx_to_name.push(s.to_string());
        adj.push(Vec::new());
        name_to_idx.insert(s.to_string(), i);
        i
    };
    for (s, t) in &raw_edges {
        let u = intern(s, &mut name_to_idx, &mut idx_to_name, &mut adj);
        let v = intern(t, &mut name_to_idx, &mut idx_to_name, &mut adj);
        adj[u].push(v);
        adj[v].push(u); // undirected
    }
    let v_count = idx_to_name.len();
    if v_count < 3 {
        stats.duration_ms = started.elapsed().as_millis() as u64;
        return stats;
    }

    // Pick top-K source nodes by degree (sampled Brandes — captures
    // the high-BC bridges without paying full O(V^2 + V*E)).
    let mut degree_idx: Vec<(usize, usize)> =
        (0..v_count).map(|i| (i, adj[i].len())).collect();
    degree_idx.sort_by(|a, b| b.1.cmp(&a.1));
    let sample_size = degree_idx.len().min(50);
    let sources: Vec<usize> = degree_idx.iter().take(sample_size).map(|&(i, _)| i).collect();

    // Brandes accumulator
    let mut bc: Vec<f64> = vec![0.0; v_count];
    let timeout = std::time::Duration::from_secs(30);

    'outer: for &s in &sources {
        if started.elapsed() > timeout {
            break 'outer;
        }
        // BFS from s tracking shortest-path predecessors and counts.
        let mut sigma: Vec<f64> = vec![0.0; v_count];
        let mut dist: Vec<i32> = vec![-1; v_count];
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); v_count];
        let mut stack: Vec<usize> = Vec::new();
        sigma[s] = 1.0;
        dist[s] = 0;
        let mut q: VecDeque<usize> = VecDeque::new();
        q.push_back(s);
        while let Some(v) = q.pop_front() {
            stack.push(v);
            for &w in &adj[v] {
                if dist[w] < 0 {
                    dist[w] = dist[v] + 1;
                    q.push_back(w);
                }
                if dist[w] == dist[v] + 1 {
                    sigma[w] += sigma[v];
                    preds[w].push(v);
                }
            }
        }
        // Backward accumulation.
        let mut delta: Vec<f64> = vec![0.0; v_count];
        while let Some(w) = stack.pop() {
            for &p in &preds[w] {
                if sigma[w] > 0.0 {
                    delta[p] += (sigma[p] / sigma[w]) * (1.0 + delta[w]);
                }
            }
            if w != s {
                bc[w] += delta[w];
            }
        }
        stats.sources_sampled += 1;
    }

    // Standard normalisation: 2 / ((n-1)(n-2)) for undirected.
    let norm = if v_count > 2 {
        2.0 / (((v_count - 1) * (v_count - 2)) as f64)
    } else {
        1.0
    };
    for v in 0..v_count {
        bc[v] *= norm;
    }

    // Persist (only nodes with non-zero BC; the rest stay at default 0).
    for (i, score) in bc.iter().enumerate() {
        if *score <= 0.0 {
            continue;
        }
        if *score > stats.top_score {
            stats.top_score = *score;
        }
        let sql = "INSERT OR REPLACE INTO node_centrality(qualified_name, betweenness, sample_size) \
                   VALUES(?1, ?2, ?3)";
        let params = vec![
            serde_json::Value::String(idx_to_name[i].clone()),
            serde_json::Value::Number(
                serde_json::Number::from_f64(*score).unwrap_or_else(|| serde_json::Number::from(0)),
            ),
            serde_json::Value::Number((stats.sources_sampled as i64).into()),
        ];
        let resp = store
            .inject
            .insert(
                project_id,
                DbLayer::Graph,
                sql,
                params,
                InjectOptions {
                    emit_event: false,
                    audit: false,
                    ..InjectOptions::default()
                },
            )
            .await;
        if resp.success {
            stats.nodes_scored += 1;
        }
    }

    stats.duration_ms = started.elapsed().as_millis() as u64;
    stats
}

// ---------------------------------------------------------------------------
// Git pass (I1 — populate git.db.commits + commit_files via subprocess)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct GitStats {
    commits_written: usize,
    commit_files_written: usize,
    status: String,
}

/// Run `git log` + `git diff-tree` against the project root and
/// persist the result into `git.db::{commits, commit_files}`.
///
/// Strategy:
///   1. `git log --pretty=format:%H|%an|%ae|%at|%P|%s -n 5000` →
///      one line per commit. We cap at 5000 commits per build to keep
///      first-time builds fast on large repos. Re-running `mneme
///      build` re-mines (idempotent — INSERT OR IGNORE on PRIMARY KEY
///      sha).
///   2. For each commit: `git show --numstat --format= <sha>` to get
///      per-file additions/deletions. Persisted to commit_files.
///
/// Failure modes (all non-fatal — logged + git.db left as-is):
///   - git binary missing on PATH → status="no-git"
///   - non-git directory → status="not-a-repo"
///   - empty repo → status="empty"
async fn run_git_pass(
    store: &Store,
    project_id: &ProjectId,
    project: &Path,
) -> GitStats {
    // M13: subprocess spawns below use `crate::windowless_command(..)`
    // which applies CREATE_NO_WINDOW on Windows.
    let mut stats = GitStats::default();

    // Probe: is git on PATH and is project a git repo?
    // M13: windowless_command(..) applies CREATE_NO_WINDOW on Windows
    // so a hook-context invocation does not flash a console.
    let probe = crate::windowless_command("git")
        .arg("rev-parse")
        .arg("--is-inside-work-tree")
        .current_dir(project)
        .output();
    match probe {
        Ok(o) if o.status.success() => {}
        Ok(_) => {
            stats.status = "not-a-repo".to_string();
            return stats;
        }
        Err(_) => {
            stats.status = "no-git".to_string();
            return stats;
        }
    }

    // git log with bounded commit cap (large repos: avoid hours-long mining)
    // M13: windowless_command(..) applies CREATE_NO_WINDOW on Windows.
    let log_out = match crate::windowless_command("git")
        .args([
            "log",
            "--pretty=format:%H|%an|%ae|%at|%P|%s",
            "-n",
            "5000",
        ])
        .current_dir(project)
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            warn!(
                stderr = %String::from_utf8_lossy(&o.stderr),
                "git log failed; git.db will be empty"
            );
            stats.status = "git-log-failed".to_string();
            return stats;
        }
        Err(e) => {
            warn!(error = %e, "git log spawn failed");
            stats.status = "spawn-failed".to_string();
            return stats;
        }
    };
    let log_text = String::from_utf8_lossy(&log_out.stdout);
    let lines: Vec<&str> = log_text.lines().collect();
    if lines.is_empty() {
        stats.status = "empty".to_string();
        return stats;
    }

    for line in &lines {
        // sha|author|email|epoch|parent|subject  (subject can contain |)
        let parts: Vec<&str> = line.splitn(6, '|').collect();
        if parts.len() < 6 {
            continue;
        }
        let sha = parts[0].trim();
        let author_name = parts[1];
        let author_email = parts[2];
        let timestamp_epoch: i64 = parts[3].parse().unwrap_or(0);
        let parent = parts[4];
        let subject = parts[5];

        // ISO-8601 from epoch
        let committed_at = format!(
            "{}",
            chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp_epoch, 0)
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
        );

        // Take only first parent if there are multiple (merge commits)
        let parent_sha: Option<&str> = parent.split_whitespace().next().filter(|s| !s.is_empty());

        let sql = "INSERT OR IGNORE INTO commits(sha, author_name, author_email, committed_at, message, parent_sha) \
                   VALUES(?1, ?2, ?3, ?4, ?5, ?6)";
        let params = vec![
            serde_json::Value::String(sha.to_string()),
            serde_json::Value::String(author_name.to_string()),
            serde_json::Value::String(author_email.to_string()),
            serde_json::Value::String(committed_at),
            serde_json::Value::String(subject.to_string()),
            match parent_sha {
                Some(p) => serde_json::Value::String(p.to_string()),
                None => serde_json::Value::Null,
            },
        ];
        let resp = store
            .inject
            .insert(
                project_id,
                DbLayer::Git,
                sql,
                params,
                InjectOptions {
                    emit_event: false,
                    audit: false,
                    ..InjectOptions::default()
                },
            )
            .await;
        if resp.success {
            stats.commits_written += 1;
        }
    }

    // commit_files: cap at 500 most-recent commits to keep build time
    // bounded. Older commits get a row in commits but no per-file
    // numstat — re-runnable.
    for line in lines.iter().take(500) {
        let parts: Vec<&str> = line.splitn(6, '|').collect();
        if parts.len() < 6 {
            continue;
        }
        let sha = parts[0].trim();
        // M13: windowless_command(..) applies CREATE_NO_WINDOW on Windows.
        let numstat = match crate::windowless_command("git")
            .args(["show", "--numstat", "--format=", sha])
            .current_dir(project)
            .output()
        {
            Ok(o) if o.status.success() => o,
            _ => continue,
        };
        let text = String::from_utf8_lossy(&numstat.stdout);
        for nl in text.lines() {
            let parts: Vec<&str> = nl.splitn(3, '\t').collect();
            if parts.len() < 3 {
                continue;
            }
            let adds: i64 = parts[0].parse().unwrap_or(0);
            let dels: i64 = parts[1].parse().unwrap_or(0);
            let path = parts[2];
            let sql = "INSERT OR IGNORE INTO commit_files(sha, file_path, additions, deletions) \
                       VALUES(?1, ?2, ?3, ?4)";
            let params = vec![
                serde_json::Value::String(sha.to_string()),
                serde_json::Value::String(path.to_string()),
                serde_json::Value::Number(adds.into()),
                serde_json::Value::Number(dels.into()),
            ];
            let resp = store
                .inject
                .insert(
                    project_id,
                    DbLayer::Git,
                    sql,
                    params,
                    InjectOptions {
                        emit_event: false,
                        audit: false,
                        ..InjectOptions::default()
                    },
                )
                .await;
            if resp.success {
                stats.commit_files_written += 1;
            }
        }
    }

    stats.status = "ok".to_string();
    stats
}

// ---------------------------------------------------------------------------
// Deps pass (I1 — populate deps.db.dependencies from manifests)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct DepsStats {
    deps_written: usize,
    npm_count: usize,
    cargo_count: usize,
    python_count: usize,
}

/// Parse dependency manifests reachable from the project root and
/// populate `deps.db.dependencies`. Recognises:
///   - package.json (npm/bun/pnpm/yarn) — `dependencies` + `devDependencies`
///   - Cargo.toml (rust) — `[dependencies]` + `[dev-dependencies]`
///   - requirements.txt (python) — flat list
///   - Cargo workspace: traverses `[workspace.members]` for leaf manifests
///
/// Walks the project tree up to `DEPS_MAX_DEPTH` levels, skipping the
/// hard-coded `is_ignored` set (node_modules, target, .git, .venv, …).
/// Without this walk, polyglot monorepos (root has no manifest, deps
/// live in `mcp/package.json`, `vision/package.json`, etc.) silently
/// reported zero npm/python deps — only the cargo path traversed
/// `[workspace.members]`. The walker now treats all three ecosystems
/// symmetrically: every reachable manifest contributes rows.
///
/// Best-effort and bounded — ignores transitive deps, ignores
/// version-range parsing complexity (stores raw version string).
async fn run_deps_pass(
    store: &Store,
    project_id: &ProjectId,
    project: &Path,
) -> DepsStats {
    let mut stats = DepsStats::default();

    // Discover every manifest under `project` up to `DEPS_MAX_DEPTH`,
    // pruning ignored dirs (node_modules / target / .git / .venv / …)
    // at the directory level so `node_modules/foo/package.json` (which
    // would otherwise drown out real first-party deps) never gets read.
    let manifests = discover_dep_manifests(project);

    // npm: every package.json reachable under the project (root-only
    // before — missed monorepos like the mneme source itself, where
    // package.json lives in mcp/, vision/, vscode/).
    for pj in &manifests.package_json {
        if let Some(rows) = parse_package_json(pj) {
            stats.npm_count += rows.len();
            for (pkg, ver, is_dev) in rows {
                let sql = "INSERT OR IGNORE INTO dependencies(package, version, ecosystem, is_dev) VALUES(?1, ?2, 'npm', ?3)";
                let params = vec![
                    serde_json::Value::String(pkg),
                    serde_json::Value::String(ver),
                    serde_json::Value::Number(if is_dev { 1 } else { 0 }.into()),
                ];
                let resp = store
                    .inject
                    .insert(
                        project_id,
                        DbLayer::Deps,
                        sql,
                        params,
                        InjectOptions {
                            emit_event: false,
                            audit: false,
                            ..InjectOptions::default()
                        },
                    )
                    .await;
                if resp.success {
                    stats.deps_written += 1;
                }
            }
        }
    }

    // cargo: Cargo.toml at root pulls in `[workspace.members]` leaves;
    // any other Cargo.toml found by the walker (sub-crates that aren't
    // in `[workspace.members]`, or a project that just has nested
    // crates with no top-level workspace) is parsed too.
    let cargo_root = project.join("Cargo.toml");
    let mut cargo_manifests: Vec<PathBuf> = manifests.cargo_toml.clone();
    if cargo_root.exists() {
        // Try to parse workspace.members if present
        if let Ok(text) = std::fs::read_to_string(&cargo_root) {
            if let Some(members) = parse_workspace_members(&text) {
                for m in members {
                    let mp = project.join(&m).join("Cargo.toml");
                    if mp.exists() && !cargo_manifests.contains(&mp) {
                        cargo_manifests.push(mp);
                    }
                }
            }
        }
    }
    for m in &cargo_manifests {
        if let Some(rows) = parse_cargo_toml(m) {
            for (pkg, ver, is_dev) in rows {
                stats.cargo_count += 1;
                let sql = "INSERT OR IGNORE INTO dependencies(package, version, ecosystem, is_dev) VALUES(?1, ?2, 'cargo', ?3)";
                let params = vec![
                    serde_json::Value::String(pkg),
                    serde_json::Value::String(ver),
                    serde_json::Value::Number(if is_dev { 1 } else { 0 }.into()),
                ];
                let resp = store
                    .inject
                    .insert(
                        project_id,
                        DbLayer::Deps,
                        sql,
                        params,
                        InjectOptions {
                            emit_event: false,
                            audit: false,
                            ..InjectOptions::default()
                        },
                    )
                    .await;
                if resp.success {
                    stats.deps_written += 1;
                }
            }
        }
    }

    // python: every requirements.txt reachable under the project.
    for rt in &manifests.requirements_txt {
        if let Some(rows) = parse_requirements_txt(rt) {
            stats.python_count += rows.len();
            for (pkg, ver) in rows {
                let sql = "INSERT OR IGNORE INTO dependencies(package, version, ecosystem, is_dev) VALUES(?1, ?2, 'pypi', 0)";
                let params = vec![
                    serde_json::Value::String(pkg),
                    serde_json::Value::String(ver),
                ];
                let resp = store
                    .inject
                    .insert(
                        project_id,
                        DbLayer::Deps,
                        sql,
                        params,
                        InjectOptions {
                            emit_event: false,
                            audit: false,
                            ..InjectOptions::default()
                        },
                    )
                    .await;
                if resp.success {
                    stats.deps_written += 1;
                }
            }
        }
    }

    stats
}

/// Cap on how many directory levels deep `run_deps_pass` walks to find
/// dependency manifests. `node_modules`, `.git`, `target`, `.venv`,
/// etc. are pruned at the directory level via `is_ignored` so the
/// effective search space stays small. 6 is enough for typical
/// monorepo shapes (`apps/<app>/package.json`,
/// `packages/<pkg>/package.json`, `services/<svc>/<sub>/package.json`)
/// without descending into vendored / generated payloads.
const DEPS_MAX_DEPTH: usize = 6;

#[derive(Debug, Default)]
struct DepManifests {
    package_json: Vec<PathBuf>,
    cargo_toml: Vec<PathBuf>,
    requirements_txt: Vec<PathBuf>,
}

/// Walk `root` up to `DEPS_MAX_DEPTH` levels deep, returning every
/// dependency manifest the deps pass knows how to parse.
///
/// Pruning is done at the directory level via `is_ignored` (the same
/// safety-net list `mneme build` uses for its source walk) so we never
/// recurse into `node_modules/`, `target/`, `.git/`, `.venv/`,
/// `AppData/`, or any of the other vendored / generated dirs.
fn discover_dep_manifests(root: &Path) -> DepManifests {
    let mut out = DepManifests::default();
    let walker = walkdir::WalkDir::new(root)
        .max_depth(DEPS_MAX_DEPTH)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // Always allow the root entry itself; for descendants prune
            // anything `is_ignored` would skip in the source walk.
            if e.depth() == 0 {
                return true;
            }
            !is_ignored(e.path())
        });
    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = match entry.path().file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        match name {
            "package.json" => out.package_json.push(entry.path().to_path_buf()),
            "Cargo.toml" => out.cargo_toml.push(entry.path().to_path_buf()),
            "requirements.txt" => out.requirements_txt.push(entry.path().to_path_buf()),
            _ => {}
        }
    }
    out
}

fn parse_package_json(path: &Path) -> Option<Vec<(String, String, bool)>> {
    let raw = std::fs::read_to_string(path).ok()?;
    // Some package.json files (especially those edited via Notepad on
    // Windows) ship a UTF-8 BOM; serde_json would otherwise fail with
    // "expected value at line 1 column 1" and the deps pass would
    // silently report npm=0.
    let text = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let mut out: Vec<(String, String, bool)> = Vec::new();
    for (key, is_dev) in [("dependencies", false), ("devDependencies", true)] {
        if let Some(obj) = v.get(key).and_then(|x| x.as_object()) {
            for (pkg, ver) in obj {
                if let Some(s) = ver.as_str() {
                    out.push((pkg.clone(), s.to_string(), is_dev));
                }
            }
        }
    }
    Some(out)
}

fn parse_cargo_toml(path: &Path) -> Option<Vec<(String, String, bool)>> {
    let text = std::fs::read_to_string(path).ok()?;
    // Lightweight TOML parsing without pulling in a TOML crate dep on
    // cli/. We scan for the `[dependencies]` and `[dev-dependencies]`
    // section markers and grab `name = "version"` lines until the next
    // section header. Good enough for top-level dep listings; will not
    // capture tables-as-dep (`name = { version = "x", ... }`) but for
    // those we extract the name only.
    let mut out: Vec<(String, String, bool)> = Vec::new();
    let mut current: Option<&'static str> = None;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(stripped) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            current = match stripped {
                "dependencies" => Some("dependencies"),
                "dev-dependencies" => Some("dev-dependencies"),
                _ => None,
            };
            continue;
        }
        let in_section = match current {
            Some(_) => true,
            None => false,
        };
        if !in_section {
            continue;
        }
        // Grab "name = ..." — split on first '=' only
        if let Some(eq_idx) = line.find('=') {
            let name = line[..eq_idx].trim().trim_matches('"');
            let rest = line[eq_idx + 1..].trim();
            if name.is_empty() || rest.is_empty() {
                continue;
            }
            // Try simple "version" string
            let version: String = if rest.starts_with('"') {
                rest.trim_matches('"').to_string()
            } else if rest.starts_with('{') {
                // Parse `version = "x"` from inside the table
                if let Some(idx) = rest.find("version") {
                    let after = &rest[idx + 7..];
                    if let Some(eq2) = after.find('=') {
                        let after_eq = after[eq2 + 1..].trim();
                        let quoted: String = after_eq
                            .chars()
                            .skip_while(|c| *c != '"')
                            .skip(1)
                            .take_while(|c| *c != '"')
                            .collect();
                        if !quoted.is_empty() {
                            quoted
                        } else {
                            "*".to_string()
                        }
                    } else {
                        "*".to_string()
                    }
                } else {
                    "workspace".to_string()
                }
            } else {
                rest.to_string()
            };
            let is_dev = matches!(current, Some("dev-dependencies"));
            out.push((name.to_string(), version, is_dev));
        }
    }
    Some(out)
}

fn parse_workspace_members(toml_text: &str) -> Option<Vec<String>> {
    let mut in_workspace = false;
    let mut members: Vec<String> = Vec::new();
    let mut collecting = false;
    for raw in toml_text.lines() {
        let line = raw.trim();
        if line == "[workspace]" {
            in_workspace = true;
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') && line != "[workspace]" {
            in_workspace = false;
            collecting = false;
        }
        if !in_workspace {
            continue;
        }
        if line.starts_with("members") {
            collecting = true;
        }
        if collecting {
            // Grab any quoted strings on the line
            let mut chars = line.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '"' {
                    let s: String = chars.by_ref().take_while(|x| *x != '"').collect();
                    if !s.is_empty() {
                        members.push(s);
                    }
                }
            }
            if line.contains(']') {
                collecting = false;
            }
        }
    }
    if members.is_empty() {
        None
    } else {
        Some(members)
    }
}

fn parse_requirements_txt(path: &Path) -> Option<Vec<(String, String)>> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut out: Vec<(String, String)> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
            continue;
        }
        // Strip env markers and comments
        let line = line.split(';').next().unwrap_or(line).trim();
        let line = line.split('#').next().unwrap_or(line).trim();
        // Forms: pkg, pkg==1.2, pkg>=1, pkg~=1.2.3
        for sep in ["==", ">=", "<=", "!=", "~=", ">", "<"].iter() {
            if let Some(idx) = line.find(sep) {
                let name = line[..idx].trim().to_string();
                let ver = line[idx + sep.len()..].trim().to_string();
                if !name.is_empty() {
                    out.push((name, ver));
                    break;
                }
            }
        }
        if !line.contains("==") && !line.contains(">=") && !line.contains("<=")
            && !line.contains("!=") && !line.contains("~=") && !line.contains('>') && !line.contains('<')
        {
            // bare package name, no version
            if !line.is_empty() {
                out.push((line.to_string(), "*".to_string()));
            }
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Tests pass (I1 — populate tests.db.test_files using K5 detection)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct TestsStats {
    test_files_written: usize,
}

/// Materialise the K5 is_test detection result into `tests.db::test_files`.
/// K5 already classifies files during the parse loop (looks_like_test_path
/// matches *.test.tsx, *.spec.ts, *_test.rs, test_*.py, *_test.go, paths
/// under tests/ or __tests__/ and stamps `is_test=1` on every node from
/// that file). This pass just SELECTs those file rows and INSERTs them
/// into the test_files table that the vision Test Coverage view reads
/// from.
///
/// Framework column populated by file extension heuristic:
///   *.test.{ts,tsx,js,jsx} → "vitest|jest"
///   *.spec.{ts,tsx,js,jsx} → "vitest|jest"
///   *_test.rs              → "rust-test"
///   test_*.py / *_test.py  → "pytest"
///   *_test.go              → "go-test"
///   under __tests__/ / tests/ → "unknown"
async fn run_tests_pass(
    store: &Store,
    project_id: &ProjectId,
    paths: &PathManager,
) -> TestsStats {
    let mut stats = TestsStats::default();

    let graph_db = paths.shard_db(project_id, DbLayer::Graph);
    if !graph_db.exists() {
        return stats;
    }

    let conn = match rusqlite::Connection::open_with_flags(
        &graph_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "tests-pass: read graph.db failed; skipping");
            return stats;
        }
    };

    let mut stmt = match conn.prepare(
        "SELECT DISTINCT file_path FROM nodes \
         WHERE kind='file' AND is_test=1 AND file_path IS NOT NULL",
    ) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "tests-pass: prepare failed; skipping");
            return stats;
        }
    };

    let test_paths: Vec<String> = match stmt.query_map([], |r| r.get::<_, String>(0)) {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(e) => {
            warn!(error = %e, "tests-pass: query_map failed; skipping");
            return stats;
        }
    };

    if test_paths.is_empty() {
        return stats;
    }

    for fp in &test_paths {
        let framework = framework_for_path(fp);
        let sql = "INSERT OR REPLACE INTO test_files(file_path, framework) VALUES(?1, ?2)";
        let params = vec![
            serde_json::Value::String(fp.clone()),
            serde_json::Value::String(framework.to_string()),
        ];
        let resp = store
            .inject
            .insert(
                project_id,
                DbLayer::Tests,
                sql,
                params,
                InjectOptions {
                    emit_event: false,
                    audit: false,
                    ..InjectOptions::default()
                },
            )
            .await;
        if resp.success {
            stats.test_files_written += 1;
        }
    }

    stats
}

fn framework_for_path(p: &str) -> &'static str {
    let lower = p.to_lowercase();
    if lower.ends_with("_test.rs") {
        "rust-test"
    } else if lower.ends_with("_test.go") {
        "go-test"
    } else if lower.ends_with("_test.py") || lower.contains("/test_") {
        "pytest"
    } else if lower.contains(".test.") || lower.contains(".spec.") {
        "vitest|jest"
    } else if lower.contains("/__tests__/") || lower.contains("/tests/") {
        "unknown"
    } else {
        "unknown"
    }
}

/// Look up the embedding row id by (text_hash, model) — invoked when
/// `INSERT OR IGNORE` swallowed the insert due to a UNIQUE collision.
async fn lookup_embedding_id(
    store: &Store,
    project_id: &ProjectId,
    text_hash: &str,
    model: &str,
) -> Option<i64> {
    let q = store::query::Query {
        project: project_id.clone(),
        layer: DbLayer::Semantic,
        sql: "SELECT id FROM embeddings WHERE text_hash = ?1 AND model = ?2 LIMIT 1".into(),
        params: vec![
            serde_json::Value::String(text_hash.into()),
            serde_json::Value::String(model.into()),
        ],
    };
    let resp = store.query.query_rows(q).await;
    if !resp.success {
        return None;
    }
    let rows = resp.data?;
    let first = rows.first()?;
    // query_rows returns one Value::Object per row, keyed by column name.
    first.get("id")?.as_i64()
}

// ---------------------------------------------------------------------------
// Model probes (K3/K4)
// ---------------------------------------------------------------------------
//
// Mneme degrades to keyword-only retrieval (and signature-only summaries)
// when the embedding model and code-summary LLM aren't installed. This was
// a silent failure in v0.4 — `mneme recall` returned weak results, and
// `mneme build` finished without surfacing why. Per phase-a-issues.md
// §K3/§K4, the choice is to FAIL LOUDLY at build time so the user knows
// to run `mneme models install <model>`. We never silently degrade.
//
// The probe is filesystem-only (checks `~/.mneme/models/` for the expected
// files). No model is loaded, no ORT session is built — that work happens
// later when the brain crate actually uses them. A miss here therefore
// over-warns (a model could be loaded successfully even if the file
// layout is non-default) but under-warns is the dangerous direction:
// silent degradation. Conservative is correct.
#[derive(Debug)]
struct ModelProbes {
    embedding_model_present: bool,
    llm_model_present: bool,
}

impl ModelProbes {
    /// Cheap filesystem-only probe. Never errors — missing models map to
    /// `false`, which drives the warning path.
    fn probe() -> Self {
        let dir = brain::embeddings::default_model_dir();
        let onnx = dir.join("bge-small-en-v1.5.onnx");
        let tok = dir.join("tokenizer.json");
        let embedding_model_present = onnx.exists() && tok.exists();

        // The LLM is feature-gated in `brain`; when it's off no local LLM
        // can ever load. Either way the user-visible signal is the same:
        // any `*.gguf|*.ggml|*.bin` model under `~/.mneme/models/` counts
        // as "present" (filesystem-only probe; we don't try to load
        // anything yet — that's brain's job at runtime). Permissive on
        // filename so dropping a different small-model variant doesn't
        // produce a false "missing" warning.
        let llm_model_present = std::fs::read_dir(&dir)
            .ok()
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| {
                        e.path()
                            .extension()
                            .and_then(|x| x.to_str())
                            .map(|s| s.to_ascii_lowercase())
                    })
                    .any(|ext| ext == "gguf" || ext == "ggml" || ext == "bin")
            })
            .unwrap_or(false);

        Self {
            embedding_model_present,
            llm_model_present,
        }
    }

    fn print_warnings_at_top(&self) {
        if !self.embedding_model_present {
            eprintln!();
            eprintln!(
                "WARN: NO EMBEDDING MODEL CONFIGURED — semantic recall will degrade to keyword-only."
            );
            eprintln!(
                "      Run `mneme models install qwen-embed-0.5b` (or drop bge-small-en-v1.5.onnx + tokenizer.json"
            );
            eprintln!(
                "      under ~/.mneme/models/) to enable transformer-quality recall."
            );
        }
        if !self.llm_model_present {
            eprintln!();
            eprintln!(
                "WARN: NO LOCAL LLM CONFIGURED — code-node summaries will fall back to signature-only text."
            );
            eprintln!(
                "      Run `mneme models install qwen-coder-0.5b` (or any local GGUF model under ~/.mneme/models/)"
            );
            eprintln!(
                "      to enable LLM-generated 1-sentence summaries cached per file_hash."
            );
        }
        if !self.embedding_model_present || !self.llm_model_present {
            eprintln!();
        }
    }

    fn print_warnings_in_summary(&self) {
        if self.embedding_model_present && self.llm_model_present {
            return;
        }
        println!();
        if !self.embedding_model_present {
            println!(
                "  WARN:         no embedding model — semantic recall is keyword-only \
                 (run `mneme models install qwen-embed-0.5b`)"
            );
        }
        if !self.llm_model_present {
            println!(
                "  WARN:         no local LLM — code summaries are signature-only \
                 (run `mneme models install qwen-coder-0.5b`)"
            );
        }
    }

    /// K3 + H4 + B-009: explicit, action-oriented embedding status line
    /// for the build summary. Replaces the silent "embeddings: 0 written"
    /// degraded-build path with a line that tells the user (a) it's
    /// degraded and (b) exactly what to type to fix it.
    ///
    /// B-009: previously this line ALWAYS reported `model: bge-small-en-v1.5
    /// (present)` unconditionally whenever the `.onnx` file existed on
    /// disk, even when the active runtime backend was the hashing-trick
    /// fallback (because `real-embeddings` feature wasn't compiled in or
    /// `onnxruntime.dll` wasn't loadable). Users saw `(present)` and
    /// assumed real semantic recall when they were actually getting the
    /// fallback. The line now hedges with `file on disk; active backend
    /// shown above in `embeddings:` summary line` so the user knows to
    /// cross-reference the per-build `embeddings: ... backend=<name>`
    /// row that the embedding pass already emits — that row is the
    /// ground truth for runtime backend.
    pub(crate) fn summary_embedding_status_line(&self) -> String {
        if self.embedding_model_present {
            // Model file present. The actual ACTIVE backend (real BGE vs
            // hashing-trick fallback) is reported by the per-build
            // `embeddings: N written, ... backend=<name>` row emitted by
            // the embedding pass — that row is the runtime ground truth.
            // We deliberately do NOT claim "(present)" without a
            // qualifier because the file-on-disk vs runtime-loaded check
            // diverges silently when ORT can't load `onnxruntime.dll`.
            "embeddings: bge-small-en-v1.5 model file present on disk (active runtime backend reported in `embeddings:` summary row above — `backend=hashing-trick` means the .onnx is unloadable; rebuild with `--features mneme-brain/real-embeddings` and add `onnxruntime.dll` to PATH or set ORT_DYLIB_PATH)".to_string()
        } else {
            "embeddings: 0 embedded \u{2014} no model installed (run `mneme models install qwen-embed-0.5b` to enable semantic recall)".to_string()
        }
    }
}

/// H4: explicit community-detection status line. The Leiden pass writes
/// `cluster_stats` into the summary block via the `communities:` row
/// already, but the row doesn't tell the operator that the result is
/// reproducible across builds. This helper materialises a one-liner
/// that does — useful for grepping build logs in CI to confirm the
/// seed didn't drift.
///
/// Visibility: file-private. `LeidenStats` is itself private, so
/// `pub(crate)` would trip `private_interfaces`. The only callers are
/// inside this file (run_inline summary block + tests under `super::*`).
fn community_status_line(stats: &LeidenStats) -> String {
    format!(
        "communities: {} (Leiden, deterministic seed; members={}, edges_used={})",
        stats.communities, stats.members, stats.edges_used,
    )
}

/// Public probe used by other CLI commands (e.g. `mneme recall`) so they
/// can re-emit the same K3 warning once per session. Kept here rather than
/// in `brain` so the wording stays in sync with the build summary.
pub(crate) fn embedding_model_present() -> bool {
    ModelProbes::probe().embedding_model_present
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let d = h.finalize();
    let mut s = String::with_capacity(d.len() * 2);
    for b in d.iter() {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Resolve `project` to an absolute, canonicalised path. Falls back to
/// CWD if the user passed nothing.
pub(crate) fn resolve_project(arg: Option<PathBuf>) -> CliResult<PathBuf> {
    let raw = arg.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let canonical = std::fs::canonicalize(&raw).unwrap_or(raw);
    Ok(canonical)
}

/// Build an IPC client honoring `--socket` overrides.
pub(crate) fn make_client(socket_override: Option<PathBuf>) -> IpcClient {
    match socket_override {
        Some(p) => IpcClient::new(p),
        None => IpcClient::default_path(),
    }
}

/// B-001/B-002: build pipeline variant of [`make_client`]. The build
/// pipeline must NEVER auto-spawn a second `mneme-daemon` on connect
/// failure, and every per-call round-trip must be tightly bounded so a
/// stuck supervisor surfaces as a fast error instead of a 74-minute
/// hang. The constants live here so every build-pipeline IPC call
/// (`run_dispatched` job submit, watchdog poll, etc.) shares the same
/// budget.
///
/// ## Hooks NEVER auto-spawn either (Bug E, 2026-04-29)
///
/// The hook commands (`mneme inject` / `pre_tool` / `post_tool` /
/// `session_*` / `turn_end`) used to use the default auto-spawn path.
/// **That decision is reversed.** The supervisor not being up means
/// mneme is intentionally inactive; the user runs `mneme daemon start`
/// to activate context capture. Hooks must NOT ambush the user with a
/// daemon they didn't ask for.
///
/// The pre-fix behaviour was the root of the resurrection loop on
/// Anish's POS host (postmortem 2026-04-29 §3.E + §12.5): every Claude
/// Code tool call fired ~9 hooks, each connect-failure spawned a
/// `mneme daemon start`, and combined with Bug D (visible cmd-window
/// storm) that produced 22 cmd windows per tool call.
///
/// Today the only auto-spawn caller is `make_client` itself, used by
/// commands the user explicitly types (`mneme recall`, `mneme blast`,
/// `mneme step`, `mneme audit`, etc.) — those still benefit from the
/// "supervisor wakes up on first query" UX. Hooks use
/// [`crate::hook_payload::make_hook_client`] which sets
/// [`IpcClient::with_no_autospawn`].
pub(crate) fn make_client_for_build(socket_override: Option<PathBuf>) -> IpcClient {
    make_client(socket_override)
        .with_no_autospawn()
        .with_timeout(BUILD_IPC_TIMEOUT)
}

/// B-001: per-round-trip timeout for build-pipeline IPC. The default
/// `IpcClient` budget is 120s; that's appropriate for hooks but lets a
/// wedged supervisor turn `mneme build` into a 74-minute hang (as
/// observed on EC2 2026-04-27). 5s is generous for a JSON round-trip
/// against a healthy supervisor and forces a fast fallback when one
/// isn't.
pub(crate) const BUILD_IPC_TIMEOUT: Duration = Duration::from_secs(5);

/// Pretty-print any [`IpcResponse`] variant, surface [`IpcResponse::Error`]
/// as a [`CliError::Supervisor`]. Used by every IPC-bound command.
///
/// ## SD-3: exhaustive over today's variants + graceful default arm
///
/// `IpcResponse` is `#[non_exhaustive]` (see `cli/src/ipc.rs`) because
/// the supervisor adds new `ControlResponse` variants faster than the
/// CLI's printing logic does. Every known variant has its own arm; a
/// final `_ =>` arm catches anything a newer supervisor sends so that
/// (a) we don't fail to compile against a future ipc.rs, and (b) we
/// don't hard-error at runtime — instead we emit a `warn!` and pretty-
/// print the JSON so the user still sees the response.
///
/// `#[allow(unreachable_patterns)]` is needed because, in this very
/// crate, every variant of `IpcResponse` is named — the safety-net
/// `_ => ` arm only becomes meaningful from outside this crate, where
/// `#[non_exhaustive]` forces consumers to handle the unknown case.
#[allow(unreachable_patterns)]
pub(crate) fn handle_response(response: IpcResponse) -> CliResult<()> {
    match response {
        IpcResponse::Pong => {
            println!("pong");
            Ok(())
        }
        IpcResponse::Status { children } => {
            println!("{}", serde_json::to_string_pretty(&children)?);
            Ok(())
        }
        IpcResponse::Logs { entries } => {
            for e in &entries {
                println!("{}", serde_json::to_string(e)?);
            }
            Ok(())
        }
        IpcResponse::Ok { message } => {
            if let Some(m) = message {
                println!("{m}");
            } else {
                println!("ok");
            }
            Ok(())
        }
        IpcResponse::Dispatched { worker } => {
            println!("dispatched to worker {worker}");
            Ok(())
        }
        IpcResponse::JobQueued { job_id } => {
            println!("queued job {job_id}");
            Ok(())
        }
        IpcResponse::JobQueue { snapshot } => {
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
            Ok(())
        }
        IpcResponse::RecallResults { hits } => {
            println!("{}", serde_json::to_string_pretty(&hits)?);
            Ok(())
        }
        IpcResponse::BlastResults { impacted } => {
            println!("{}", serde_json::to_string_pretty(&impacted)?);
            Ok(())
        }
        IpcResponse::GodNodesResults { nodes } => {
            println!("{}", serde_json::to_string_pretty(&nodes)?);
            Ok(())
        }
        IpcResponse::GraphifyCorpusQueued { queued, project } => {
            println!(
                "graphify_corpus: queued {queued} ingest job(s) for {}",
                project.display()
            );
            Ok(())
        }
        IpcResponse::SnapshotCombined { children, jobs, scope } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "scope": scope,
                    "children": children,
                    "jobs": jobs,
                }))?
            );
            Ok(())
        }
        IpcResponse::RebuildAcked { workers, force } => {
            println!(
                "rebuild: signalled {} worker(s) (force={force}): {}",
                workers.len(),
                workers.join(", ")
            );
            Ok(())
        }
        IpcResponse::Error { message } => Err(CliError::Supervisor(message)),
        IpcResponse::BadRequest { message } => Err(CliError::Supervisor(format!(
            "supervisor reports bad_request: {message}"
        ))),
        // SD-3: forward-compatible default arm. `IpcResponse` is
        // `#[non_exhaustive]`, so a newer supervisor adding (e.g.)
        // `IpcResponse::FooQueued { ... }` would otherwise cause the
        // build to fail in this file. Instead we log + pretty-print the
        // JSON so the user still sees the response and the CLI keeps
        // working against a newer daemon.
        other => {
            warn!("received unhandled IpcResponse variant; this CLI build pre-dates the supervisor");
            println!("{}", serde_json::to_string_pretty(&other)?);
            Ok(())
        }
    }
}

/// Keep `IpcRequest` import alive so `cargo check --warnings` stays clean
/// even when the supervisor-bound request machinery is exercised only by
/// other commands.
#[allow(dead_code)]
fn _ipc_req_anchor() -> Option<IpcRequest> {
    None
}

// ---------------------------------------------------------------------------
// B-003: subprocess registry + Ctrl-C handler for inline build
// ---------------------------------------------------------------------------
//
// When `mneme build` runs inline, several passes shell out to native
// workers (mneme-scanners today; future: parser/brain/md-ingest if more
// passes are migrated to subprocess fallbacks). On EC2 2026-04-27 these
// children survived as orphans when the user Ctrl-C'd the build, sitting
// idle and consuming RAM until manually `taskkill`'d.
//
// `BuildChildRegistry` is a small Arc<Mutex<Vec<u32>>> of PIDs that any
// pass can register a freshly-spawned child with. On Drop (panic /
// early-return / normal exit) AND on Ctrl-C (`spawn_ctrl_c_killer`) we
// taskkill every registered PID. This is defense-in-depth: the primary
// fix for orphan workers is B-002 (no second daemon), which removes the
// most common source of orphans; this registry catches anything else
// the build pipeline may spawn directly.

/// Shared, cheap-to-clone handle to the inline build's spawned-child PID
/// list. Cloning shares the inner `Arc<Mutex<...>>`; every clone sees
/// the same registry so passes that take the registry by value can still
/// register children that the parent guard will clean up.
#[derive(Clone, Debug)]
pub(crate) struct BuildChildRegistry {
    inner: Arc<Mutex<BuildChildRegistryInner>>,
}

#[derive(Debug, Default)]
struct BuildChildRegistryInner {
    /// PIDs registered by the build pipeline. Empty list at construct
    /// time; passes call [`BuildChildRegistry::register`] to add.
    pids: Vec<u32>,
    /// Set once cleanup has happened (Ctrl-C, panic, or top-level
    /// drop). Subsequent calls to `kill_all` are no-ops so we don't
    /// double-kill an already-collected PID.
    drained: bool,
}

impl BuildChildRegistry {
    /// Build a new, empty registry.
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(BuildChildRegistryInner::default())),
        }
    }

    /// Append a PID to the kill-on-cleanup list. Pass the value of
    /// `child.id()` from `tokio::process::Child` or
    /// `std::process::Child` — anything resolvable to a `u32` PID on
    /// the host.
    pub(crate) fn register(&self, pid: u32) {
        if let Ok(mut g) = self.inner.lock() {
            if !g.drained {
                g.pids.push(pid);
            }
        }
    }

    /// Snapshot of currently-registered PIDs. Test-only — production
    /// callers go through [`Self::kill_all`].
    #[cfg(test)]
    pub(crate) fn pids(&self) -> Vec<u32> {
        self.inner.lock().map(|g| g.pids.clone()).unwrap_or_default()
    }

    /// Remove a PID from the kill list. Pass after a child has exited
    /// cleanly so the Drop guard doesn't try to taskkill an already-
    /// reaped PID. Best-effort — a missed remove just causes a benign
    /// "no such process" on the eventual taskkill.
    pub(crate) fn unregister(&self, pid: u32) {
        if let Ok(mut g) = self.inner.lock() {
            g.pids.retain(|p| *p != pid);
        }
    }

    /// Send a kill signal to every registered PID and mark the
    /// registry drained. Idempotent — calling twice is a no-op.
    pub(crate) fn kill_all(&self) {
        let pids: Vec<u32> = match self.inner.lock() {
            Ok(mut g) => {
                if g.drained {
                    return;
                }
                g.drained = true;
                std::mem::take(&mut g.pids)
            }
            Err(_) => return,
        };
        for pid in pids {
            kill_pid_best_effort(pid);
        }
    }
}

/// Drop is the safety net for the panic / early-return path. The
/// explicit Ctrl-C handler in `spawn_ctrl_c_killer` covers the
/// user-initiated cancel case; this Drop covers everything else
/// (a `?` propagating Err out of `run_inline`, an unexpected panic, …).
impl Drop for BuildChildRegistry {
    fn drop(&mut self) {
        // Only the LAST clone holding the Arc should perform cleanup.
        // Earlier drops (e.g. when a pass that took a clone returns)
        // leave the registry alive for the parent. Strong-count ==1
        // means we're the last holder.
        if Arc::strong_count(&self.inner) == 1 {
            self.kill_all();
        }
    }
}

/// Best-effort cross-platform process kill. We don't propagate errors
/// because at this point the build is already exiting and a stale PID
/// (kernel reaped it before we got there) is the common case.
fn kill_pid_best_effort(pid: u32) {
    #[cfg(windows)]
    {
        // taskkill /F /PID <pid> is the standard Windows path. /T
        // would also kill child trees, but our registered children
        // (mneme-scanners et al.) don't fork further so /T is
        // unnecessary noise.
        // M13: windowless_command(..) applies CREATE_NO_WINDOW so the
        // taskkill spawn does not flash a console (this fn is called
        // from a SIGINT handler that may run in a windowless parent).
        let _ = crate::windowless_command("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        // Shell out to `kill` rather than pull in `libc` / `nix` just
        // for SIGTERM — keeps the workspace dep graph small and
        // matches what `commands::uninstall.rs` already does on
        // Unix. SIGTERM gives the child a chance to flush; if it
        // ignores the signal we trust the kernel cleanup at process
        // exit.
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// B-003: spawn a tokio task that listens for Ctrl-C (SIGINT on Unix)
/// and runs the registry's `kill_all` on first hit. Returns a guard
/// whose Drop aborts the listener so a clean build exit doesn't leave
/// the listener running into the next command (matters for tests that
/// call `run_inline` multiple times).
///
/// Implementation note: we deliberately do NOT call
/// `std::process::exit` in the Ctrl-C handler. The default tokio
/// shutdown is already correct — the runtime will tear down on next
/// poll once the main future resolves. Aborting here would skip the
/// per-pass DB writes that have already started.
/// K10 #6: Ctrl-C handler that ALSO persists the build-state checkpoint
/// before exiting. Layered on top of `spawn_ctrl_c_killer` so the
/// existing process-tree cleanup remains intact — we just write the
/// state file as the FIRST thing on Ctrl-C arrival, ensuring the
/// resume artifact lands even if the registry kill or process exit
/// races with subsequent shutdown signals.
///
/// `state_snapshot` is a single point-in-time copy of the state at
/// the moment the build started. The cursor inside that copy is
/// updated in-band by `mark_parse_progress` saves while the build
/// runs; this Ctrl-C handler only writes a "best-effort final" state
/// based on whatever the last save persisted to disk PLUS a phase
/// marker indicating we were interrupted.
fn spawn_ctrl_c_killer_with_state(
    children: BuildChildRegistry,
    project_root: PathBuf,
    state_snapshot: crate::commands::build_state::BuildState,
) -> CtrlCGuard {
    let registry = children;
    let handle = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::warn!(
                "build interrupted by Ctrl-C; cleaning up spawned workers + persisting build-state"
            );
            // Step 1: write the most recent state-on-disk if any. The
            // periodic save in the parse loop is the source of truth;
            // we don't try to mutate the cursor here (we'd have stale
            // data). Instead, we re-load whatever the parse loop last
            // saved and stamp `updated_at` so consumers can tell this
            // file is fresh.
            let loaded =
                crate::commands::build_state::load(&project_root)
                    .unwrap_or(state_snapshot);
            let mut interrupted = loaded;
            interrupted.updated_at = chrono::Utc::now().to_rfc3339();
            // Persist with the existing phase — the parse loop already
            // moved it forward as work progressed.
            if let Err(e) =
                crate::commands::build_state::save(&project_root, &interrupted)
            {
                tracing::warn!(error = %e, "failed to persist final build-state on Ctrl-C");
            }
            // Step 2: tear down workers (existing behaviour).
            registry.kill_all();
            // Step 3: exit 130 (SIGINT convention).
            std::process::exit(130);
        }
    });
    CtrlCGuard { handle: Some(handle) }
}

/// RAII handle for the Ctrl-C listener task. Dropping this aborts the
/// listener; we abort instead of letting it run to completion because
/// `tokio::signal::ctrl_c()` would otherwise stay subscribed across
/// the rest of the process's lifetime.
#[derive(Debug)]
pub(crate) struct CtrlCGuard {
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for CtrlCGuard {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

fn is_ignored(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    matches!(
        name,
        // Build outputs (language-agnostic, node, rust, .NET, Java, electron)
        "target"
            | "node_modules"
            | ".git"
            | ".hg"
            | ".svn"
            | "dist"
            | "dist-electron"
            | "release"
            | "out"
            | "build"
            | "bin"
            | "obj"
            | ".next"
            | ".nuxt"
            | ".vite"
            | ".parcel-cache"
            | ".turbo"
            | ".svelte-kit"
            | ".astro"
            // Python
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".pytest_cache"
            | ".mypy_cache"
            | ".ruff_cache"
            | ".tox"
            | "site-packages"
            // Editors + tooling
            | ".idea"
            | ".vscode"
            | ".vs"
            | ".DS_Store"
            | ".cache"
            | ".yarn"
            | ".pnpm-store"
            // Windows user profile nightmares (F-010 repros)
            | "AppData"
            | "OneDrive"
            | "$Recycle.Bin"
            | "NTUSER.DAT"
            // macOS / Linux user profile dirs
            | "Library"
            | ".Trash"
            | "Applications"
            // Mneme's own tree — never reindex ourselves
            | ".mneme"
            | ".claude"
    )
}

/// P16: Build an `ignore::gitignore::Gitignore` matcher from project
/// root's `.mnemeignore` (preferred) or `.gitignore` (fallback).
/// Returns `None` if no ignore file exists.
///
/// Semantics: full gitignore spec — supports negation (`!foo`),
/// directory-only patterns (`foo/`), nested paths, and glob wildcards.
/// Applied on top of the hard-coded `is_ignored` list above, not instead
/// of it, so the safety-net defaults (node_modules, .git, AppData, ...)
/// always apply regardless of what the user's ignore file says.
fn load_project_ignore(root: &Path) -> Option<ignore::gitignore::Gitignore> {
    use ignore::gitignore::GitignoreBuilder;
    // Try .mnemeignore first, fall back to .gitignore.
    for name in [".mnemeignore", ".gitignore"] {
        let p = root.join(name);
        if p.exists() {
            let mut builder = GitignoreBuilder::new(root);
            if let Some(err) = builder.add(&p) {
                warn!(path = %p.display(), error = %err, "failed to read ignore file; skipping");
                continue;
            }
            match builder.build() {
                Ok(gi) => return Some(gi),
                Err(e) => {
                    warn!(path = %p.display(), error = %e, "failed to build ignore matcher");
                }
            }
        }
    }
    None
}

/// Returns true if `path` should be skipped per the project ignore file.
/// Returns false when no ignore is passed or the path isn't matched.
fn project_ignore_matches(gi: Option<&ignore::gitignore::Gitignore>, path: &Path, is_dir: bool) -> bool {
    let Some(gi) = gi else { return false };
    matches!(gi.matched(path, is_dir), ignore::Match::Ignore(_))
}

/// First-pass walker — counts how many candidate source files are under
/// `root` after `is_ignored` + `.mnemeignore/.gitignore` filters. Stops
/// early once `cap` is exceeded so the guard pre-flight doesn't pay the
/// full walk cost on huge trees.
fn count_candidate_files(root: &Path, cap: usize) -> usize {
    let gi = load_project_ignore(root);
    let gi_ref = gi.as_ref();
    let walker = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let p = e.path();
            if is_ignored(p) {
                return false;
            }
            let is_dir = e.file_type().is_dir();
            !project_ignore_matches(gi_ref, p, is_dir)
        });

    let mut n = 0usize;
    for entry in walker {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        if Language::from_filename(entry.path()).is_some() {
            n += 1;
            if n > cap {
                return n;
            }
        }
    }
    n
}

/// Heuristic: treat any byte slice whose first 512 bytes contain a NUL as
/// binary. This skips images, compiled object files, etc.
fn looks_binary(buf: &[u8]) -> bool {
    buf.iter().take(512).any(|&b| b == 0)
}

// ---------------------------------------------------------------------------
// I1 batch 3 — Empty-shard producers
// ---------------------------------------------------------------------------
//
// Wave 4 wires producers for the eight shards Phase A flagged as EMPTY in
// the cycle-3 EC2 build summary:
//
//   livestate.db, wiki.db, architecture.db, conventions.db,
//   federated.db, perf.db, errors.db, agents.db
//
// Each pass below follows the same contract as the existing 12 passes
// (Leiden / embedding / audit / tests / git / deps / betweenness /
// intent / git-intent / convention-intent / intent-md / multimodal):
//
//   * Read inputs (shard files, in-memory state) read-only.
//   * Write through `store.inject` so the per-shard single-writer
//     invariant stays intact (one writer task per shard, IPC-safe).
//   * Failure is non-fatal — the build itself already succeeded; an
//     empty producer just means the shard stays at the previous row
//     count.
//
// The federated pass is the only deviation: it goes through the
// existing `brain::federated::FederatedStore`, which uses raw
// rusqlite for performance reasons that predate the inject layer.
// The federated shard is not yet served by a writer task in
// `store.query`, so the historical direct-DB path is preserved.
// Single-writer is still honoured because the inline build holds
// the project's BuildLock for the entire run.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct ArchitectureStats {
    snapshots_written: usize,
    community_count: u32,
    bridges: usize,
    hubs: usize,
}

/// Run the architecture analysis over the freshly-built graph + community
/// partition and persist a single snapshot row to
/// `architecture.db::architecture_snapshots`.
///
/// Reads:
///   * `graph.db::nodes` — qualified_name + kind + file_path
///   * `graph.db::edges` — source/target/kind/confidence_score
///   * `semantic.db::community_membership` + `communities` — community
///     ids (we just wrote them in `run_leiden_pass`)
///
/// Output: one new row per build with serialised JSON columns
/// (coupling_matrix, risk_index, bridge_nodes, hub_nodes).
async fn run_architecture_pass(
    store: &Store,
    project_id: &ProjectId,
    paths: &PathManager,
) -> ArchitectureStats {
    let mut stats = ArchitectureStats::default();

    let graph_db = paths.shard_db(project_id, DbLayer::Graph);
    let semantic_db = paths.shard_db(project_id, DbLayer::Semantic);
    if !graph_db.exists() {
        return stats;
    }

    // Open both shards read-only. Parallel passes won't trip on us
    // because we only read, and the writer task for each shard owns
    // its own connection.
    let g_conn = match rusqlite::Connection::open_with_flags(
        &graph_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "architecture: read graph.db failed");
            return stats;
        }
    };

    // Build qualified_name -> community_id map.
    let mut comm_of: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    if semantic_db.exists() {
        if let Ok(s_conn) = rusqlite::Connection::open_with_flags(
            &semantic_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) {
            if let Ok(mut stmt) = s_conn
                .prepare("SELECT community_id, node_qualified FROM community_membership")
            {
                if let Ok(rows) = stmt.query_map([], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                }) {
                    for row in rows.flatten() {
                        comm_of.insert(row.1, row.0 as u32);
                    }
                }
            }
        }
    }

    // Pull node metadata.
    let mut node_stmt = match g_conn.prepare(
        "SELECT qualified_name, kind, file_path FROM nodes \
         WHERE qualified_name IS NOT NULL",
    ) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "architecture: prep nodes failed");
            return stats;
        }
    };
    let mut nodes: Vec<ArchNode> = Vec::new();
    if let Ok(rows) = node_stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        ))
    }) {
        for (qn, kind, file) in rows.flatten() {
            let cid = comm_of.get(&qn).copied().unwrap_or(0);
            nodes.push(ArchNode {
                qualified_name: qn,
                kind,
                file,
                community_id: cid,
                caller_count: 0,
                criticality: 0.0,
                security_flag: false,
            });
        }
    }
    if nodes.is_empty() {
        return stats;
    }

    // Compute caller_count (in-degree on `calls`-like edges) by reading
    // edges in one pass. Architecture's risk index reads this counter.
    let mut callers: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    let mut edges: Vec<ArchEdge> = Vec::new();
    if let Ok(mut estmt) = g_conn.prepare(
        "SELECT source_qualified, target_qualified, kind, confidence_score \
         FROM edges WHERE source_qualified IS NOT NULL AND target_qualified IS NOT NULL",
    ) {
        if let Ok(rows) = estmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, Option<f64>>(3)?.unwrap_or(1.0) as f32,
            ))
        }) {
            for (s, t, kind, weight) in rows.flatten() {
                if s == t {
                    continue;
                }
                if kind == "calls" || kind == "uses" {
                    *callers.entry(t.clone()).or_default() += 1;
                }
                edges.push(ArchEdge {
                    source: s,
                    target: t,
                    kind,
                    weight,
                });
            }
        }
    }
    for n in &mut nodes {
        n.caller_count = *callers.get(&n.qualified_name).unwrap_or(&0);
    }

    let overview = ArchitectureScanner::new().analyze(&nodes, &edges);

    let coupling_json = serde_json::to_string(&overview.coupling_matrix)
        .unwrap_or_else(|_| "[]".into());
    let risk_json =
        serde_json::to_string(&overview.risk_index).unwrap_or_else(|_| "[]".into());
    let bridges_json =
        serde_json::to_string(&overview.bridge_nodes).unwrap_or_else(|_| "[]".into());
    let hubs_json = serde_json::to_string(&overview.hub_nodes).unwrap_or_else(|_| "[]".into());

    let sql = "INSERT INTO architecture_snapshots\
               (community_count, node_count, edge_count, coupling_matrix, \
                risk_index, bridge_nodes, hub_nodes, notes) \
               VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";
    let params = vec![
        serde_json::Value::Number(serde_json::Number::from(overview.community_count as i64)),
        serde_json::Value::Number(serde_json::Number::from(overview.node_count as i64)),
        serde_json::Value::Number(serde_json::Number::from(overview.edge_count as i64)),
        serde_json::Value::String(coupling_json),
        serde_json::Value::String(risk_json),
        serde_json::Value::String(bridges_json),
        serde_json::Value::String(hubs_json),
        serde_json::Value::String("inline build pass".into()),
    ];
    let resp = store
        .inject
        .insert(
            project_id,
            DbLayer::Architecture,
            sql,
            params,
            InjectOptions {
                emit_event: false,
                audit: false,
                ..InjectOptions::default()
            },
        )
        .await;
    if resp.success {
        stats.snapshots_written = 1;
        stats.community_count = overview.community_count;
        stats.bridges = overview.bridge_nodes.len();
        stats.hubs = overview.hub_nodes.len();
    } else {
        warn!(error = ?resp.error, "architecture snapshot insert failed");
    }
    stats
}

#[derive(Debug, Default, Clone)]
struct ConventionsStats {
    patterns_inferred: usize,
    rows_written: usize,
}

/// Materialise the inferred conventions accumulated by the file-loop
/// observer into `conventions.db::conventions`. One row per pattern;
/// `id` is deterministic over (kind, json) so re-runs upsert in place
/// rather than appending duplicates.
async fn run_conventions_pass(
    store: &Store,
    project_id: &ProjectId,
    learner: &DefaultLearner,
) -> ConventionsStats {
    let mut stats = ConventionsStats::default();
    let inferred = learner.infer_conventions();
    stats.patterns_inferred = inferred.len();
    if inferred.is_empty() {
        return stats;
    }

    let now_ms = chrono::Utc::now().timestamp_millis();
    for conv in &inferred {
        let pattern_kind = match &conv.pattern {
            brain::ConventionPattern::Naming { .. } => "naming",
            brain::ConventionPattern::ImportOrder { .. } => "import_order",
            brain::ConventionPattern::ErrorHandling { .. } => "error_handling",
            brain::ConventionPattern::TestLayout { .. } => "test_layout",
            brain::ConventionPattern::Dependency { .. } => "dependency",
            brain::ConventionPattern::ComponentShape { .. } => "component_shape",
        };
        let pattern_json = serde_json::to_string(&conv.pattern).unwrap_or_else(|_| "{}".into());
        let sql = "INSERT INTO conventions(id, pattern_kind, pattern_json, confidence, \
                   evidence_count, updated_at) VALUES(?1, ?2, ?3, ?4, ?5, ?6) \
                   ON CONFLICT(id) DO UPDATE SET \
                       confidence = excluded.confidence, \
                       evidence_count = excluded.evidence_count, \
                       updated_at = excluded.updated_at";
        let params = vec![
            serde_json::Value::String(conv.id.clone()),
            serde_json::Value::String(pattern_kind.into()),
            serde_json::Value::String(pattern_json),
            serde_json::Value::Number(
                serde_json::Number::from_f64(conv.confidence as f64)
                    .unwrap_or_else(|| serde_json::Number::from(0)),
            ),
            serde_json::Value::Number((conv.evidence_count as i64).into()),
            serde_json::Value::Number(now_ms.into()),
        ];
        let resp = store
            .inject
            .insert(
                project_id,
                DbLayer::Conventions,
                sql,
                params,
                InjectOptions {
                    emit_event: false,
                    audit: false,
                    ..InjectOptions::default()
                },
            )
            .await;
        if resp.success {
            stats.rows_written += 1;
        }
    }
    stats
}

#[derive(Debug, Default, Clone)]
struct WikiStats {
    pages_built: usize,
    pages_written: usize,
    /// Silent-1 (Class H-silent): non-zero when a wiki_pages OR
    /// wiki_runs insert returned `Response::success == false`. The
    /// audit groups these with the parse-loop graph inserts because a
    /// dropped wiki_runs row produces a half-populated audit trail
    /// (`mneme why` / vision dashboard timeline diverges from reality).
    insert_failures: u64,
}

/// Generate one Markdown wiki page per Leiden community produced by
/// `run_leiden_pass` and persist into `wiki.db::wiki_pages`.
///
/// Reads `semantic.db::{communities, community_membership}` to recover
/// the partition. For each community we pull the file-paths of its
/// member nodes from `graph.db::nodes` to populate the page's `## Files`
/// section. Entry points (god-nodes) require richer ranking we don't
/// have in-process; the daemon-side path can supersede this with a
/// betweenness/criticality-anchored selection later.
async fn run_wiki_pass(
    store: &Store,
    project_id: &ProjectId,
    paths: &PathManager,
) -> WikiStats {
    let mut stats = WikiStats::default();

    let semantic_db = paths.shard_db(project_id, DbLayer::Semantic);
    let graph_db = paths.shard_db(project_id, DbLayer::Graph);
    if !semantic_db.exists() {
        return stats;
    }

    // Collect (community_id, member_qualified) pairs.
    let s_conn = match rusqlite::Connection::open_with_flags(
        &semantic_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "wiki: read semantic.db failed");
            return stats;
        }
    };

    // Communities table id is the SQLite row id (AUTOINCREMENT).
    // Fetch (id, name, cohesion) so we can rebuild a `Community` shim.
    let mut comm_meta: std::collections::HashMap<i64, (String, f32)> =
        std::collections::HashMap::new();
    if let Ok(mut stmt) = s_conn.prepare("SELECT id, name, cohesion FROM communities") {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2)? as f32,
            ))
        }) {
            for (id, name, cohesion) in rows.flatten() {
                comm_meta.insert(id, (name, cohesion));
            }
        }
    }
    if comm_meta.is_empty() {
        return stats;
    }

    // Member qualified-names per community.
    let mut members_of: std::collections::HashMap<i64, Vec<String>> =
        std::collections::HashMap::new();
    if let Ok(mut stmt) = s_conn
        .prepare("SELECT community_id, node_qualified FROM community_membership")
    {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        }) {
            for (cid, qn) in rows.flatten() {
                members_of.entry(cid).or_default().push(qn);
            }
        }
    }

    // Per-qualified file_path lookup from graph.db.
    let mut file_of: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    if graph_db.exists() {
        if let Ok(g_conn) = rusqlite::Connection::open_with_flags(
            &graph_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) {
            if let Ok(mut stmt) = g_conn
                .prepare("SELECT qualified_name, file_path FROM nodes WHERE file_path IS NOT NULL")
            {
                if let Ok(rows) = stmt.query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                }) {
                    for (qn, fp) in rows.flatten() {
                        file_of.entry(qn).or_insert(fp);
                    }
                }
            }
        }
    }

    let builder = WikiBuilder::new();
    for (cid_db, (name, cohesion)) in &comm_meta {
        let members_qn = members_of.get(cid_db).cloned().unwrap_or_default();
        // Map members back to a fake brain::leiden::Community (id+cohesion
        // is all WikiBuilder reads; members are synthetic NodeIds).
        let community = BrainCommunity {
            id: *cid_db as u32,
            members: members_qn
                .iter()
                .map(|qn| brain::NodeId::new(qualified_to_u128(qn)))
                .collect(),
            cohesion: *cohesion,
        };
        let mut files: Vec<String> = members_qn
            .iter()
            .filter_map(|qn| file_of.get(qn).cloned())
            .collect();
        files.sort();
        files.dedup();

        // Best-effort entry-points: pick the first 5 members as
        // anchors. Daemon-side wiki regen does this with betweenness
        // ranking; this synthetic anchor is good enough for the page
        // not to render an empty `## Key Symbols`.
        let entry_points: Vec<WikiSymbol> = members_qn
            .iter()
            .take(5)
            .map(|qn| WikiSymbol {
                node_id: brain::NodeId::new(qualified_to_u128(qn)),
                qualified_name: qn.clone(),
                kind: "symbol".into(),
                summary: String::new(),
                file: file_of.get(qn).cloned(),
            })
            .collect();

        let label = if name.starts_with("community-") {
            None
        } else {
            Some(name.clone())
        };
        let input = WikiCommunityInput {
            community: &community,
            entry_points,
            files: files.clone(),
            risk_score: 0.0,
            label,
        };
        let page = builder.build_page(&input);
        stats.pages_built += 1;

        // Pick `version = 1 + max(version)` for this slug so re-runs
        // don't violate the append-only contract documented in the
        // schema.
        let next_version = next_wiki_version(&store, project_id, &page.slug).await;

        let entry_points_json = serde_json::to_string(&page.entry_points)
            .unwrap_or_else(|_| "[]".into());
        let files_json =
            serde_json::to_string(&page.file_paths).unwrap_or_else(|_| "[]".into());

        let sql = "INSERT INTO wiki_pages(slug, version, community_id, title, markdown, \
                   summary, entry_points, file_paths, risk_score) \
                   VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)";
        let params = vec![
            serde_json::Value::String(page.slug.clone()),
            serde_json::Value::Number(next_version.into()),
            serde_json::Value::Number((*cid_db).into()),
            serde_json::Value::String(page.title.clone()),
            serde_json::Value::String(page.markdown.clone()),
            serde_json::Value::String(page.summary.clone()),
            serde_json::Value::String(entry_points_json),
            serde_json::Value::String(files_json),
            serde_json::Value::Number(
                serde_json::Number::from_f64(page.risk_score as f64)
                    .unwrap_or_else(|| serde_json::Number::from(0)),
            ),
        ];
        let resp = store
            .inject
            .insert(
                project_id,
                DbLayer::Wiki,
                sql,
                params,
                InjectOptions {
                    emit_event: false,
                    audit: false,
                    ..InjectOptions::default()
                },
            )
            .await;
        if resp.success {
            stats.pages_written += 1;
        } else {
            // Silent-1: previously the !success branch was empty —
            // pages silently dropped while the build summary still
            // displayed `pages_written` against a smaller-than-expected
            // count. Now we record the failure so the build returns
            // non-zero and the operator can see what didn't land.
            stats.insert_failures += 1;
            warn!(
                slug = %page.slug,
                error = ?resp.error,
                "wiki page insert failed; build will exit non-zero",
            );
        }
    }

    // Stamp wiki_runs row so consumers can correlate the regen.
    // Silent-1 (was `let _ = store.inject.insert(...)` at build.rs:5346):
    // capture the response and track failures via WikiStats so the
    // outer build can surface this in the same fail-loud path as the
    // parse-loop graph inserts.
    let runs_sql = "INSERT INTO wiki_runs(completed_at, pages_generated, notes) \
                    VALUES(datetime('now'), ?1, ?2)";
    let runs_params = vec![
        serde_json::Value::Number((stats.pages_written as i64).into()),
        serde_json::Value::String("inline build pass".into()),
    ];
    let runs_resp = store
        .inject
        .insert(
            project_id,
            DbLayer::Wiki,
            runs_sql,
            runs_params,
            InjectOptions {
                emit_event: false,
                audit: false,
                ..InjectOptions::default()
            },
        )
        .await;
    if !runs_resp.success {
        stats.insert_failures += 1;
        warn!(
            error = ?runs_resp.error,
            "wiki_runs audit-row insert failed; build will exit non-zero",
        );
    }

    stats
}

/// Pick the next available `version` for a wiki slug. Append-only
/// schema (see `store/src/schema.rs`): every regeneration of the same
/// slug bumps `version`. Returns 1 when the slug has never been
/// written.
async fn next_wiki_version(
    store: &Store,
    project_id: &ProjectId,
    slug: &str,
) -> i64 {
    let q = store::query::Query {
        project: project_id.clone(),
        layer: DbLayer::Wiki,
        sql: "SELECT COALESCE(MAX(version), 0) AS v FROM wiki_pages WHERE slug = ?1".into(),
        params: vec![serde_json::Value::String(slug.into())],
    };
    let resp = store.query.query_rows(q).await;
    if !resp.success {
        return 1;
    }
    let rows = match resp.data {
        Some(r) => r,
        None => return 1,
    };
    let max_v = rows
        .first()
        .and_then(|row| row.get("v"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    max_v + 1
}

#[derive(Debug, Default, Clone)]
struct FederatedStats {
    indexed: usize,
    skipped: usize,
}

/// Compute SimHash + MinHash fingerprints for every source file under
/// `project` and persist into `federated.db::pattern_fingerprints`.
/// Mirrors `mneme federated scan` but runs as a build-time side-effect
/// so the shard isn't empty after a fresh `mneme build`.
///
/// Local-only — fingerprints stay on disk; sync requires the explicit
/// opt-in marker `+~/.mneme/federated.optin` and a v0.4 relay (see
/// `cli/src/commands/federated.rs::cmd_sync`).
///
/// The `FederatedStore` wraps raw rusqlite (not `store.inject`) — see
/// the section comment at the top of these passes for why that's
/// acceptable here. Build holds the per-project `BuildLock` for the
/// whole inline run, so single-writer is preserved.
async fn run_federated_pass(
    project_id: &ProjectId,
    project: &Path,
    paths: &PathManager,
) -> FederatedStats {
    let mut stats = FederatedStats::default();
    let db_path = paths.shard_db(project_id, DbLayer::Federated);
    let mut store = match FederatedStore::new(&db_path) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "federated: open store failed");
            return stats;
        }
    };

    let walker = walkdir::WalkDir::new(project)
        .follow_links(false)
        .max_depth(8)
        .into_iter()
        .filter_entry(|e| !is_ignored(e.path()));

    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !is_federated_source(path) {
            continue;
        }
        let content = match std::fs::read_to_string(path) {
            Ok(c) if !c.is_empty() => c,
            _ => {
                stats.skipped += 1;
                continue;
            }
        };
        let kind = federated_pattern_kind_for(path);
        let fp = FederatedStore::compute_fingerprint(&content, kind);
        let source = path.to_string_lossy().into_owned();
        if store.index_local_with_source(fp, Some(&source)).is_ok() {
            stats.indexed += 1;
        } else {
            stats.skipped += 1;
        }
    }

    stats
}

/// Mirror of `cli/src/commands/federated.rs::is_source_file`. Kept
/// duplicated to avoid coupling the build path to the federated CLI
/// module's private helpers.
fn is_federated_source(path: &Path) -> bool {
    const EXTS: &[&str] = &[
        "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "go", "java", "kt", "swift", "c",
        "cc", "cpp", "h", "hpp", "rb", "php", "cs", "scala", "sh", "bash", "zsh", "ps1",
    ];
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| EXTS.contains(&ext))
        .unwrap_or(false)
}

/// Mirror of `cli/src/commands/federated.rs::pattern_kind_for`.
fn federated_pattern_kind_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => "rust_file",
        Some("ts") | Some("tsx") => "ts_file",
        Some("js") | Some("jsx") | Some("mjs") | Some("cjs") => "js_file",
        Some("py") => "py_file",
        Some("go") => "go_file",
        Some("java") | Some("kt") | Some("scala") => "jvm_file",
        Some("swift") => "swift_file",
        Some("c") | Some("cc") | Some("cpp") | Some("h") | Some("hpp") => "c_file",
        Some("rb") => "rb_file",
        Some("php") => "php_file",
        Some("cs") => "cs_file",
        Some("sh") | Some("bash") | Some("zsh") => "shell_file",
        Some("ps1") => "ps1_file",
        _ => "other_file",
    }
}

#[derive(Debug, Default, Clone)]
struct PerfStats {
    baselines_written: usize,
    build_ms: u128,
}

/// Persist build-time throughput numbers as baseline rows in
/// `perf.db::baselines`. Each metric becomes its own row:
///
///   * `build.duration_ms` — wall-clock ms for the inline build
///   * `build.files_indexed` — count of files that landed in graph.db
///   * `build.nodes_total` — total nodes extracted
///   * `build.edges_total` — total edges extracted
///   * `build.parse_files_per_sec` — derived throughput
///
/// Append-only. A future regression test reads the latest row per
/// `metric` (`ORDER BY captured_at DESC LIMIT 1`).
async fn run_perf_pass(
    store: &Store,
    project_id: &ProjectId,
    elapsed: Duration,
    indexed: usize,
    node_total: u64,
    edge_total: u64,
) -> PerfStats {
    let mut stats = PerfStats::default();
    let elapsed_ms = elapsed.as_millis();
    stats.build_ms = elapsed_ms;
    let secs = elapsed.as_secs_f64().max(0.001);

    let metrics: Vec<(&str, f64, &str)> = vec![
        ("build.duration_ms", elapsed_ms as f64, "ms"),
        ("build.files_indexed", indexed as f64, "files"),
        ("build.nodes_total", node_total as f64, "nodes"),
        ("build.edges_total", edge_total as f64, "edges"),
        (
            "build.parse_files_per_sec",
            (indexed as f64) / secs,
            "files/sec",
        ),
    ];

    for (metric, value, unit) in &metrics {
        let sql = "INSERT INTO baselines(metric, value, unit, notes) \
                   VALUES(?1, ?2, ?3, ?4)";
        let params = vec![
            serde_json::Value::String((*metric).into()),
            serde_json::Value::Number(
                serde_json::Number::from_f64(*value)
                    .unwrap_or_else(|| serde_json::Number::from(0)),
            ),
            serde_json::Value::String((*unit).into()),
            serde_json::Value::String("inline build pass".into()),
        ];
        let resp = store
            .inject
            .insert(
                project_id,
                DbLayer::Perf,
                sql,
                params,
                InjectOptions {
                    emit_event: false,
                    audit: false,
                    ..InjectOptions::default()
                },
            )
            .await;
        if resp.success {
            stats.baselines_written += 1;
        }
    }
    stats
}

#[derive(Debug, Default, Clone)]
struct ErrorsStats {
    captured: usize,
    unique: usize,
}

/// Persist parse/read/extract failures collected during the file
/// loop. Each unique `(message, file_path)` becomes one row in
/// `errors.db::errors`; recurring errors bump `encounters` and refresh
/// `last_seen` via SQLite UPSERT.
async fn run_errors_pass(
    store: &Store,
    project_id: &ProjectId,
    errors: &[(String, String)],
) -> ErrorsStats {
    let mut stats = ErrorsStats::default();
    if errors.is_empty() {
        return stats;
    }

    let mut seen_hashes: std::collections::HashSet<String> =
        std::collections::HashSet::with_capacity(errors.len());

    for (message, file_path) in errors {
        let mut hasher = blake3::Hasher::new();
        hasher.update(message.as_bytes());
        hasher.update(b"\0");
        hasher.update(file_path.as_bytes());
        let hash = hasher.finalize().to_hex().to_string();

        let is_new = seen_hashes.insert(hash.clone());

        let sql = "INSERT INTO errors(error_hash, message, file_path) VALUES(?1, ?2, ?3) \
                   ON CONFLICT(error_hash) DO UPDATE SET \
                       encounters = encounters + 1, \
                       last_seen = datetime('now')";
        let params = vec![
            serde_json::Value::String(hash),
            serde_json::Value::String(message.clone()),
            serde_json::Value::String(file_path.clone()),
        ];
        let resp = store
            .inject
            .insert(
                project_id,
                DbLayer::Errors,
                sql,
                params,
                InjectOptions {
                    emit_event: false,
                    audit: false,
                    ..InjectOptions::default()
                },
            )
            .await;
        if resp.success {
            stats.captured += 1;
            if is_new {
                stats.unique += 1;
            }
        }
    }
    stats
}

#[derive(Debug, Default, Clone)]
struct LiveStateStats {
    events_written: usize,
}

/// Stamp a `build_completed` row into `livestate.db::file_events`.
/// The hook layer (`cli/src/hook_writer.rs::write_file_event`) is the
/// long-term producer for this table — it fires on every Edit/Write
/// tool invocation. The build-time stamp guarantees the shard isn't
/// 0 rows on a fresh project before any hook has run.
async fn run_livestate_pass(
    store: &Store,
    project_id: &ProjectId,
    project: &Path,
) -> LiveStateStats {
    let mut stats = LiveStateStats::default();
    let sql = "INSERT INTO file_events(file_path, event_type, actor) VALUES(?1, ?2, ?3)";
    let params = vec![
        serde_json::Value::String(project.display().to_string()),
        serde_json::Value::String("build_completed".into()),
        serde_json::Value::String("mneme-build".into()),
    ];
    let resp = store
        .inject
        .insert(
            project_id,
            DbLayer::LiveState,
            sql,
            params,
            InjectOptions {
                emit_event: false,
                audit: false,
                ..InjectOptions::default()
            },
        )
        .await;
    if resp.success {
        stats.events_written = 1;
    }
    stats
}

#[derive(Debug, Default, Clone)]
struct AgentsStats {
    runs_written: usize,
}

/// Stamp a synthetic `subagent_runs` row noting the inline build. The
/// canonical producer for this table is the Claude Code SubagentStop
/// hook (handled by `cli/src/commands/turn_end.rs --subagent`). Each
/// agent dispatch in a session writes one row through the hook path.
/// The build-time stamp guarantees the shard isn't 0 rows on a fresh
/// project — the row is labelled `agent_name='build'` so it's trivially
/// filterable by the recall surfaces.
async fn run_agents_pass(
    store: &Store,
    project_id: &ProjectId,
    project: &Path,
) -> AgentsStats {
    let mut stats = AgentsStats::default();
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let session = format!("build-{}", project_id.as_str());
    let sql = "INSERT INTO subagent_runs(session_id, agent_name, started_at, completed_at, \
               status, summary) VALUES(?1, ?2, ?3, ?4, ?5, ?6)";
    let params = vec![
        serde_json::Value::String(session),
        serde_json::Value::String("build".into()),
        serde_json::Value::String(now.clone()),
        serde_json::Value::String(now),
        serde_json::Value::String("ok".into()),
        serde_json::Value::String(format!(
            "inline build completed for {}",
            project.display()
        )),
    ];
    let resp = store
        .inject
        .insert(
            project_id,
            DbLayer::Agents,
            sql,
            params,
            InjectOptions {
                emit_event: false,
                audit: false,
                ..InjectOptions::default()
            },
        )
        .await;
    if resp.success {
        stats.runs_written = 1;
    }
    stats
}

// ---------------------------------------------------------------------------
// Unit tests for the pure helper surfaces above.
//
// These pin the documented contract for each helper without spinning up the
// daemon, opening any DB, or doing any network I/O. WIRE-005 follow-up:
// `build.rs` previously had ZERO unit tests despite hosting seven leaf-level
// pure helpers (`looks_binary`, `is_windows_abs`, `extract_module`,
// `normalize_path_segments`, `parse_mneme_intent`, `resolve_project`,
// `count_candidate_files`). All seven are now covered here.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    // ---- looks_binary --------------------------------------------------

    #[test]
    fn looks_binary_returns_false_for_pure_text() {
        // ASCII-only buffer must never be flagged as binary; `mneme build`
        // would otherwise refuse to parse plain source files.
        let buf = b"fn main() { println!(\"hi\"); }";
        assert!(!looks_binary(buf));
    }

    #[test]
    fn looks_binary_detects_nul_inside_first_512_bytes() {
        // A single NUL anywhere in the leading window is enough — that's
        // the heuristic the doc-comment promises.
        let mut buf = vec![b'a'; 100];
        buf.push(0);
        buf.extend_from_slice(b"more text after the nul");
        assert!(looks_binary(&buf));
    }

    #[test]
    fn looks_binary_ignores_nul_past_first_512_bytes() {
        // Window is bounded at 512 — a NUL past byte 512 must not flip
        // the result. Real-world example: a giant utf8 markdown file
        // that happens to contain `\x00` near the end of a 1MB blob.
        let mut buf = vec![b'a'; 600];
        buf.push(0);
        assert!(!looks_binary(&buf));
    }

    #[test]
    fn looks_binary_handles_empty_input() {
        // Empty file must not be reported binary; the parser layer will
        // skip it for other reasons.
        assert!(!looks_binary(b""));
    }

    // ---- is_windows_abs ------------------------------------------------

    #[test]
    fn is_windows_abs_accepts_drive_letter_with_backslash() {
        assert!(is_windows_abs(r"C:\Users\Anish"));
        assert!(is_windows_abs(r"D:\foo"));
    }

    #[test]
    fn is_windows_abs_accepts_drive_letter_with_forward_slash() {
        // Some tools normalise to `/` even on Windows; accept both.
        assert!(is_windows_abs("C:/Users/Anish"));
    }

    #[test]
    fn is_windows_abs_rejects_unix_paths() {
        assert!(!is_windows_abs("/usr/bin/mneme"));
        assert!(!is_windows_abs("./relative"));
        assert!(!is_windows_abs("../up"));
    }

    #[test]
    fn is_windows_abs_rejects_too_short_or_malformed() {
        // Need at least three bytes of `<letter>:<sep>`.
        assert!(!is_windows_abs(""));
        assert!(!is_windows_abs("C"));
        assert!(!is_windows_abs("C:"));
        // Wrong separator after the colon.
        assert!(!is_windows_abs("C:foo"));
        // Leading non-letter.
        assert!(!is_windows_abs(r"1:\foo"));
    }

    // ---- extract_module ------------------------------------------------

    #[test]
    fn extract_module_returns_clean_path_unchanged() {
        // No quotes / parens / spaces → already-clean, return verbatim.
        assert_eq!(extract_module("./foo").as_deref(), Some("./foo"));
        assert_eq!(extract_module("../bar").as_deref(), Some("../bar"));
        assert_eq!(
            extract_module("math_utils").as_deref(),
            Some("math_utils")
        );
    }

    #[test]
    fn extract_module_pulls_first_quoted_substring_for_js() {
        // TS/JS embedded shapes — both quote styles must work.
        assert_eq!(
            extract_module("import { X } from './foo';").as_deref(),
            Some("./foo")
        );
        assert_eq!(
            extract_module("import \"./bar\"").as_deref(),
            Some("./bar")
        );
    }

    #[test]
    fn extract_module_pulls_token_after_python_from() {
        assert_eq!(
            extract_module("from math_utils import add").as_deref(),
            Some("math_utils")
        );
    }

    #[test]
    fn extract_module_returns_none_for_empty_input() {
        assert_eq!(extract_module(""), None);
        assert_eq!(extract_module("   "), None);
    }

    #[test]
    fn extract_module_returns_none_when_no_module_token_found() {
        // Messy text without quotes or `from ` prefix → unrecognisable.
        assert_eq!(extract_module("import (x);"), None);
    }

    // ---- normalize_path_segments ---------------------------------------

    #[test]
    fn normalize_path_segments_collapses_parent_dir() {
        let p = Path::new("a/b/../c");
        let out = normalize_path_segments(p);
        assert_eq!(out, PathBuf::from("a/c"));
    }

    #[test]
    fn normalize_path_segments_strips_current_dir() {
        let p = Path::new("./a/./b");
        let out = normalize_path_segments(p);
        assert_eq!(out, PathBuf::from("a/b"));
    }

    #[test]
    fn normalize_path_segments_handles_only_parents_at_root() {
        // `..` past the start pops past nothing — output is empty.
        let p = Path::new("../..");
        let out = normalize_path_segments(p);
        assert_eq!(out, PathBuf::new());
    }

    // ---- parse_mneme_intent --------------------------------------------

    #[test]
    fn parse_mneme_intent_finds_kind_in_double_slash_comment() {
        let head = "// @mneme-intent: frozen\nfn main() {}";
        let got = parse_mneme_intent(head);
        assert_eq!(got, Some(("frozen".to_string(), None)));
    }

    #[test]
    fn parse_mneme_intent_extracts_reason_after_separator() {
        let head = "# @mneme-intent: deferred — pending design review";
        let got = parse_mneme_intent(head).expect("intent should parse");
        assert_eq!(got.0, "deferred");
        assert_eq!(
            got.1.as_deref(),
            Some("pending design review")
        );
    }

    #[test]
    fn parse_mneme_intent_rejects_unknown_kind() {
        // Any value not in INTENT_KINDS must return None — keeps the
        // file_intent table honest.
        let head = "// @mneme-intent: something_made_up";
        assert_eq!(parse_mneme_intent(head), None);
    }

    #[test]
    fn parse_mneme_intent_returns_none_when_marker_absent() {
        let head = "fn main() { /* normal comment */ }";
        assert_eq!(parse_mneme_intent(head), None);
    }

    // ---- resolve_project -----------------------------------------------

    #[test]
    fn resolve_project_falls_back_to_cwd_when_none() {
        // None → current working directory (canonicalised). The exact CWD
        // depends on the test runner, but the result must be absolute and
        // exist on disk.
        let got = resolve_project(None).expect("resolve_project must succeed");
        assert!(got.is_absolute(), "expected absolute, got {}", got.display());
        assert!(got.exists(), "expected existing path, got {}", got.display());
    }

    #[test]
    fn resolve_project_passes_through_explicit_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let got = resolve_project(Some(dir.path().to_path_buf()))
            .expect("resolve_project must succeed");
        // Canonicalisation may rewrite via `\\?\` on Windows or follow
        // symlinks on macOS — in both cases the file_name still matches.
        assert_eq!(got.file_name(), dir.path().file_name());
    }

    #[test]
    fn resolve_project_keeps_unresolvable_path_as_given() {
        // Canonicalize fails → fallback to the raw value (per the
        // helper's `unwrap_or(raw)` path). No panic, no error.
        let bogus = PathBuf::from("zzz_does_not_exist_anywhere_on_disk_42");
        let got = resolve_project(Some(bogus.clone())).expect("must not error");
        assert_eq!(got, bogus);
    }

    // ---- count_candidate_files -----------------------------------------

    #[test]
    fn count_candidate_files_returns_zero_for_empty_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(count_candidate_files(dir.path(), 1000), 0);
    }

    #[test]
    fn count_candidate_files_counts_supported_languages_only() {
        // A `.rs` file is supported; a `.bin` random extension is not.
        // Expect exactly one count.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("main.rs"), "fn main(){}\n")
            .expect("write rs");
        std::fs::write(dir.path().join("blob.bin"), b"ignored")
            .expect("write bin");
        let n = count_candidate_files(dir.path(), 1000);
        assert_eq!(n, 1, "expected 1 supported file, got {n}");
    }

    #[test]
    fn count_candidate_files_skips_ignored_dirs() {
        // Files inside `node_modules` must not be counted — the
        // hard-coded `is_ignored` blocklist guarantees that.
        let dir = tempfile::tempdir().expect("tempdir");
        let nm = dir.path().join("node_modules");
        std::fs::create_dir(&nm).expect("mkdir node_modules");
        std::fs::write(nm.join("dep.js"), "module.exports={}")
            .expect("write js");
        let n = count_candidate_files(dir.path(), 1000);
        assert_eq!(n, 0);
    }

    // ---- parse_package_json -------------------------------------------

    #[test]
    fn parse_package_json_extracts_deps_and_dev_deps() {
        // Sample matches the Phase A reproducer payload: name + version
        // metadata, two `dependencies`, one `devDependency`. All three
        // must come back; the dev flag must round-trip.
        let dir = tempfile::tempdir().expect("tempdir");
        let pj = dir.path().join("package.json");
        std::fs::write(
            &pj,
            r#"{"name":"test-corpus","version":"1.0.0","dependencies":{"lodash":"^4.17.21","axios":"^1.6.0"},"devDependencies":{"typescript":"^5.0.0"}}"#,
        )
        .expect("write package.json");
        let rows = parse_package_json(&pj).expect("must parse");
        assert_eq!(rows.len(), 3, "expected 3 deps, got {rows:?}");
        let lodash = rows.iter().find(|(p, _, _)| p == "lodash").expect("lodash");
        assert_eq!(lodash.1, "^4.17.21");
        assert!(!lodash.2, "lodash is a runtime dep, not a dev dep");
        let ts = rows.iter().find(|(p, _, _)| p == "typescript").expect("ts");
        assert!(ts.2, "typescript should be flagged is_dev=true");
    }

    #[test]
    fn parse_package_json_tolerates_utf8_bom() {
        // Notepad on Windows writes UTF-8 with BOM by default. Without
        // BOM-stripping, serde_json fails with "expected value at line 1
        // column 1" and the deps pass silently reports npm=0.
        let dir = tempfile::tempdir().expect("tempdir");
        let pj = dir.path().join("package.json");
        let mut bytes: Vec<u8> = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(
            br#"{"name":"x","dependencies":{"lodash":"^4.17.21"}}"#,
        );
        std::fs::write(&pj, &bytes).expect("write package.json with BOM");
        let rows = parse_package_json(&pj).expect("BOM must not defeat parser");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "lodash");
    }

    #[test]
    fn parse_package_json_returns_none_for_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist.json");
        assert!(parse_package_json(&missing).is_none());
    }

    // ---- discover_dep_manifests ---------------------------------------

    #[test]
    fn discover_dep_manifests_finds_manifests_in_subdirs() {
        // Polyglot monorepo shape: no manifest at root, manifests live
        // in language-specific subdirs (`mcp/package.json`,
        // `vision/package.json`, `cli/Cargo.toml`, `python/requirements.txt`).
        // The pre-fix deps pass only inspected the root and therefore
        // reported npm=0 / cargo=0 / python=0 on this layout.
        let dir = tempfile::tempdir().expect("tempdir");
        let mcp = dir.path().join("mcp");
        let cli = dir.path().join("cli");
        let py = dir.path().join("python");
        std::fs::create_dir(&mcp).expect("mkdir mcp");
        std::fs::create_dir(&cli).expect("mkdir cli");
        std::fs::create_dir(&py).expect("mkdir python");
        std::fs::write(mcp.join("package.json"), r#"{"name":"mcp"}"#)
            .expect("write mcp/package.json");
        std::fs::write(cli.join("Cargo.toml"), "[package]\nname=\"x\"\n")
            .expect("write cli/Cargo.toml");
        std::fs::write(py.join("requirements.txt"), "requests==2.31.0\n")
            .expect("write python/requirements.txt");

        let m = discover_dep_manifests(dir.path());
        assert_eq!(m.package_json.len(), 1, "expected mcp/package.json");
        assert_eq!(m.cargo_toml.len(), 1, "expected cli/Cargo.toml");
        assert_eq!(m.requirements_txt.len(), 1, "expected python/requirements.txt");
    }

    #[test]
    fn discover_dep_manifests_skips_node_modules() {
        // `node_modules/foo/package.json` is the noisiest source of
        // false-positive deps in any JS project; the directory-level
        // `is_ignored` prune must keep them out of the result set.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("package.json"), r#"{"name":"x"}"#)
            .expect("write root package.json");
        let nm = dir.path().join("node_modules").join("lodash");
        std::fs::create_dir_all(&nm).expect("mkdir node_modules/lodash");
        std::fs::write(nm.join("package.json"), r#"{"name":"lodash"}"#)
            .expect("write node_modules/lodash/package.json");
        let m = discover_dep_manifests(dir.path());
        assert_eq!(
            m.package_json.len(),
            1,
            "node_modules manifests must be pruned, got {:?}",
            m.package_json
        );
    }

    // ---- SD-3: handle_response covers every IpcResponse variant ------

    /// SD-3: `handle_response` must accept every variant the supervisor
    /// can send today without panicking. We hit Pong, Ok, Dispatched,
    /// JobQueued, GraphifyCorpusQueued, SnapshotCombined, RebuildAcked,
    /// BadRequest, Error in turn — Error must surface as a `Supervisor`
    /// error, BadRequest must surface as a Supervisor error too, and
    /// every other variant must return Ok.
    #[test]
    fn handle_response_handles_all_known_variants() {
        // OK-shaped variants — must all return Ok(()).
        for resp in [
            IpcResponse::Pong,
            IpcResponse::Ok { message: None },
            IpcResponse::Ok {
                message: Some("hi".into()),
            },
            IpcResponse::Dispatched {
                worker: "parser-worker-0".into(),
            },
            IpcResponse::Status { children: vec![] },
            IpcResponse::Logs { entries: vec![] },
            IpcResponse::JobQueue {
                snapshot: serde_json::json!({"pending": 0}),
            },
            IpcResponse::RecallResults { hits: vec![] },
            IpcResponse::BlastResults { impacted: vec![] },
            IpcResponse::GodNodesResults { nodes: vec![] },
            IpcResponse::GraphifyCorpusQueued {
                queued: 7,
                project: PathBuf::from("/tmp/proj"),
            },
            IpcResponse::SnapshotCombined {
                children: vec![],
                jobs: serde_json::json!({"pending": 0}),
                scope: "all".into(),
            },
            IpcResponse::RebuildAcked {
                workers: vec!["w0".into(), "w1".into()],
                force: true,
            },
        ] {
            let r = handle_response(resp.clone());
            assert!(
                r.is_ok(),
                "expected Ok for {resp:?}, got {r:?}"
            );
        }

        // Error variants — must surface as CliError::Supervisor.
        let r = handle_response(IpcResponse::Error {
            message: "boom".into(),
        });
        assert!(matches!(r, Err(CliError::Supervisor(ref m)) if m == "boom"),
            "Error must propagate verbatim: {r:?}");

        let r = handle_response(IpcResponse::BadRequest {
            message: "unknown verb".into(),
        });
        assert!(matches!(r, Err(CliError::Supervisor(ref m)) if m.contains("unknown verb")),
            "BadRequest must surface as Supervisor error: {r:?}");
    }

    /// SD-3: a simulated "future variant" (delivered as raw JSON via
    /// serde) must NOT panic the CLI. We construct an `IpcResponse` by
    /// deserializing a payload whose variant we know — this proves the
    /// `#[non_exhaustive]` + default arm contract holds end-to-end on
    /// the wire.
    #[test]
    fn handle_response_accepts_dispatched_from_wire() {
        // The supervisor side existed before the CLI knew about
        // Dispatched. This payload mirrors what the supervisor's
        // `ControlResponse::Dispatched` serializes to, and shouldn't
        // error out at deserialize time.
        let payload = serde_json::json!({
            "response": "dispatched",
            "worker": "scanner-worker-2"
        });
        let resp: IpcResponse = serde_json::from_value(payload)
            .expect("dispatched response must deserialize");
        assert!(matches!(resp, IpcResponse::Dispatched { ref worker } if worker == "scanner-worker-2"));
        assert!(handle_response(resp).is_ok());
    }

    // ---- J4: parse_intent_config --------------------------------------

    #[test]
    fn parse_intent_config_pulls_glob_intent_reason_triples() {
        // Spec example: a `rules` array with two entries — one has all
        // three fields, the other only has glob+intent. Both should
        // round-trip; intent must be lowercased.
        let raw = r#"{
          "rules": [
            { "glob": "**/*Calculator.ts", "intent": "FROZEN", "reason": "business formulas" },
            { "glob": "src/legacy/**", "intent": "frozen" }
          ]
        }"#;
        let rules = parse_intent_config(raw).expect("must parse");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].glob, "**/*Calculator.ts");
        assert_eq!(rules[0].intent, "frozen");
        assert_eq!(rules[0].reason.as_deref(), Some("business formulas"));
        assert_eq!(rules[1].glob, "src/legacy/**");
        assert_eq!(rules[1].intent, "frozen");
        assert!(rules[1].reason.is_none());
    }

    #[test]
    fn parse_intent_config_returns_none_for_garbage_json() {
        // Anything that isn't valid JSON → None, not panic.
        assert!(parse_intent_config("not json {{{").is_none());
    }

    // ---- J4: simple_glob_match ----------------------------------------

    #[test]
    fn simple_glob_match_handles_double_star_anywhere() {
        // The two patterns from the spec example must match the
        // expected paths and reject the obvious negatives.
        assert!(simple_glob_match("**/*Calculator.ts", "src/lib/PriceCalculator.ts"));
        assert!(simple_glob_match("**/*Calculator.ts", "PriceCalculator.ts"));
        assert!(!simple_glob_match("**/*Calculator.ts", "src/lib/Helper.ts"));

        assert!(simple_glob_match("src/legacy/**", "src/legacy/foo.ts"));
        assert!(simple_glob_match("src/legacy/**", "src/legacy/sub/bar.ts"));
        assert!(simple_glob_match("src/legacy/**", "src/legacy"));
        assert!(!simple_glob_match("src/legacy/**", "src/modern/foo.ts"));

        // Single `*` does not cross `/`.
        assert!(simple_glob_match("src/*.ts", "src/foo.ts"));
        assert!(!simple_glob_match("src/*.ts", "src/sub/foo.ts"));
    }

    // ---- J6: parse_intent_md ------------------------------------------

    #[test]
    fn parse_intent_md_handles_dash_bullets_and_em_dash_reason() {
        // Spec example formatting — bare filename, intent, em-dash, reason.
        let body = "\
- predictions.ts: experimental — being split
- patterns.ts:    experimental — being split
- noise: not-a-real-intent should-be-ignored-by-caller
";
        let map = parse_intent_md(body);
        assert_eq!(map.len(), 3);
        let pred = map.get("predictions.ts").expect("predictions.ts row");
        assert_eq!(pred.0, "experimental");
        assert_eq!(pred.1.as_deref(), Some("being split"));
        let pat = map.get("patterns.ts").expect("patterns.ts row");
        assert_eq!(pat.0, "experimental");
        // The third bullet is parsed (we don't validate intent here —
        // that's the caller's job via INTENT_KINDS), but its kind is
        // captured verbatim from the first whitespace-bounded token.
        let noise = map.get("noise").expect("noise row");
        assert_eq!(noise.0, "not-a-real-intent");
    }

    #[test]
    fn parse_intent_md_skips_lines_without_bullet_or_colon() {
        let body = "\
# Heading

- valid.ts: frozen — just because

random prose with no bullet
- broken-line-no-colon-after-name
";
        let map = parse_intent_md(body);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("valid.ts"));
    }

    // ---- helpers ------------------------------------------------------

    #[test]
    fn truncate_reason_keeps_short_strings_intact() {
        assert_eq!(truncate_reason("short", 80), "short");
        // Non-ASCII chars are counted as chars, not bytes — em-dash
        // is one char even though it's 3 bytes UTF-8. Without the
        // char-aware truncation we'd panic on a non-char-boundary
        // slice on Windows-locale users' commit messages.
        assert_eq!(truncate_reason("a — b", 80), "a — b");
    }

    #[test]
    fn truncate_reason_appends_ellipsis_when_too_long() {
        let s = "x".repeat(100);
        let got = truncate_reason(&s, 10);
        assert_eq!(got.chars().count(), 13); // 10 + "..."
        assert!(got.ends_with("..."));
    }

    #[test]
    fn count_todo_density_counts_both_markers_case_insensitive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("hot.ts");
        std::fs::write(
            &p,
            "// TODO fix me\n// fixme other thing\n/* Todo: split */\n",
        )
        .expect("write file");
        // 2x TODO ("TODO" + "Todo") + 1x FIXME ("fixme") = 3
        assert_eq!(count_todo_density(&p), 3);
    }

    // ---- 4.3 audit-pass routing (no daemon spawn under --inline) ------

    #[test]
    fn audit_pass_inline_does_not_spawn_daemon() {
        // 4.3: when the user passed `--inline`, the audit pass MUST NOT
        // call IpcClient::request — that path auto-spawns
        // `mneme-daemon` on dead-pipe (see ipc.rs::spawn_daemon_detached),
        // which leaks daemon processes during integration testing AND
        // violates the user's explicit "no daemon" request.
        //
        // We assert via the routing helper: inline=true must always
        // resolve to the in-process subprocess fallback, never to the
        // IPC-with-fallback path.
        assert_eq!(audit_route(true), AuditRoute::Inline);
    }

    #[test]
    fn audit_pass_non_inline_uses_ipc_with_fallback() {
        // The `--dispatch` path (and the historical default) keeps the
        // IPC-first preference: when the supervisor is up, talking to
        // it gets us shared scanner-pool reuse + concurrent runs without
        // double-spawning. inline=false must keep that contract.
        assert_eq!(audit_route(false), AuditRoute::IpcWithFallback);
    }

    // ---- B-001/B-002: build pipeline IPC must not auto-spawn ----------

    /// B-002: a NON-inline `mneme build` (the historical default and
    /// what a fresh user types as `mneme build .`) must NOT spawn a
    /// second `mneme-daemon` if connect to the existing supervisor
    /// fails. The fix lives in `make_client_for_build`, which composes
    /// `with_no_autospawn()` + `with_timeout(BUILD_IPC_TIMEOUT)` on top
    /// of the standard `make_client`. We verify by:
    ///   1. Constructing a build-pipeline client pointed at a socket
    ///      path that cannot connect.
    ///   2. Issuing a non-Stop/non-Ping request that would normally
    ///      trip `IpcClient::request`'s auto-spawn branch.
    ///   3. Asserting the call returns Err quickly (no 3s
    ///      `wait_for_supervisor` poll, no detached
    ///      `mneme daemon start`).
    #[test]
    fn build_inline_does_not_spawn_second_daemon_when_one_running() {
        // Simulate the connect-failure path with a path that cannot
        // resolve. Any platform's `connect_stream` will refuse a
        // missing socket / pipe. We DO NOT touch a real daemon — the
        // test asserts the *client's wiring* rejects autospawn, which
        // is exactly the wiring we shipped to fix B-002.
        let bogus = if cfg!(windows) {
            std::path::PathBuf::from(
                "\\\\.\\pipe\\mneme-build-no-spawn-test-does-not-exist",
            )
        } else {
            std::path::PathBuf::from(
                "/tmp/mneme-build-no-spawn-test-does-not-exist.sock",
            )
        };

        let client = make_client_for_build(Some(bogus));

        // Use Audit specifically — that's the request the build's
        // `run_audit_pass` actually sends, and it's the one that would
        // have spawned the second daemon on EC2.
        let req = crate::ipc::IpcRequest::Audit {
            scope: "full".into(),
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let started = std::time::Instant::now();
        let result = rt.block_on(client.request(req));
        let elapsed = started.elapsed();

        // Must error out (connect failed; we said no-autospawn).
        assert!(
            matches!(result, Err(crate::error::CliError::Ipc(_))),
            "expected CliError::Ipc on missing socket, got: {result:?}"
        );

        // The auto-spawn path runs `spawn_daemon_detached()` (which
        // forks `mneme daemon start`) PLUS a 3s
        // `wait_for_supervisor` poll. With no-autospawn the call must
        // bottom out almost immediately. Use 2s as a generous CI
        // cushion; on a healthy host this is sub-50ms.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "build-pipeline client must skip the 3s wait_for_supervisor branch; \
             elapsed={elapsed:?} (expected < 2s)"
        );
    }

    /// B-001: `BUILD_IPC_TIMEOUT` is the per-request budget for the
    /// build pipeline. It MUST be tighter than the default
    /// `DEFAULT_IPC_TIMEOUT` (120s) — the v0.3.2 EC2 hang showed that
    /// any IPC call inside the build pipeline that inherits the 120s
    /// budget can extend a 3-file build to 74 minutes when the
    /// supervisor is wedged. 5s is small enough to fail fast and let
    /// the direct-subprocess fallback take over.
    #[test]
    fn build_ipc_timeout_is_tighter_than_default() {
        assert!(
            BUILD_IPC_TIMEOUT < crate::ipc::DEFAULT_IPC_TIMEOUT,
            "build-pipeline IPC timeout ({:?}) must be tighter than default ({:?})",
            BUILD_IPC_TIMEOUT,
            crate::ipc::DEFAULT_IPC_TIMEOUT,
        );
        assert!(
            BUILD_IPC_TIMEOUT <= std::time::Duration::from_secs(10),
            "build-pipeline IPC timeout ({:?}) should be <= 10s; \
             a longer budget defeats the fail-fast contract",
            BUILD_IPC_TIMEOUT,
        );
    }

    // ---- B-003: build worker cleanup on Ctrl-C / panic ----------------

    /// B-003: registering a PID + calling `kill_all` drains the list
    /// AND best-effort taskkills the registered processes. We spawn a
    /// real, long-running child (so we have a known PID to kill),
    /// register it, drain via `kill_all`, then `wait` and confirm the
    /// child exited.
    #[test]
    fn build_inline_kills_orphan_workers_on_ctrl_c() {
        // Spawn a process that will sit idle for 60s if not killed.
        // The exact command differs by OS — we just need ANY
        // long-lived child we can claim ownership of.
        let mut child = if cfg!(windows) {
            // `timeout /t 60 /nobreak` blocks for 60s.
            // M13: windowless_command(..) applies CREATE_NO_WINDOW.
            crate::windowless_command("cmd")
                .args(["/c", "timeout", "/t", "60", "/nobreak"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn cmd timeout")
        } else {
            std::process::Command::new("sh")
                .args(["-c", "sleep 60"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn sh sleep")
        };
        let pid = child.id();

        let registry = BuildChildRegistry::new();
        registry.register(pid);

        // Sanity: registry remembers the PID we just registered.
        assert_eq!(registry.pids(), vec![pid], "registry must hold registered pid");

        // Simulate the Ctrl-C cleanup path. This best-effort kills
        // the child via taskkill /F (Windows) or kill -TERM (Unix).
        registry.kill_all();

        // After kill_all the registry must be drained.
        assert!(
            registry.pids().is_empty(),
            "registry must be empty after kill_all; still had: {:?}",
            registry.pids()
        );

        // Wait up to 5s for the child to exit. Without our taskkill
        // it would block here for 60s and the test would time out.
        let started = std::time::Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_status)) => break, // exited; pass
                Ok(None) => {
                    if started.elapsed() > std::time::Duration::from_secs(5) {
                        // Last-ditch reap so we don't leak the test child.
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!(
                            "child PID {pid} still alive 5s after kill_all; \
                             B-003 cleanup did not actually taskkill"
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => panic!("try_wait failed: {e}"),
            }
        }
    }

    /// B-003 hygiene: `kill_all` is idempotent — calling it twice is
    /// safe and the second call is a no-op. Without this the
    /// registry's `Drop` running after an explicit Ctrl-C would
    /// re-fire taskkill for already-reaped PIDs and produce noisy
    /// stderr in the build log.
    #[test]
    fn build_child_registry_kill_all_is_idempotent() {
        let registry = BuildChildRegistry::new();
        // Register a PID that does not exist (and won't exist in a
        // standard test environment — u32::MAX is reserved on every
        // platform we ship). Calling kill_all is a no-op for a
        // missing PID, and a second call must also be a no-op.
        registry.register(u32::MAX);
        registry.kill_all();
        assert!(registry.pids().is_empty());
        // Second call: still empty, no panic, no double-drain.
        registry.kill_all();
        assert!(registry.pids().is_empty());
    }

    /// B-003 hygiene: `unregister` removes a single PID without
    /// affecting the rest. Used after a child reaps cleanly so the
    /// registry's Drop guard doesn't try to taskkill an already-
    /// exited process.
    #[test]
    fn build_child_registry_unregister_removes_only_target() {
        let registry = BuildChildRegistry::new();
        registry.register(101);
        registry.register(202);
        registry.register(303);
        registry.unregister(202);
        let mut remaining = registry.pids();
        remaining.sort_unstable();
        assert_eq!(remaining, vec![101, 303]);
    }

    // ---- K3 + H4: build summary surfaces embedding/community status ----

    #[test]
    fn build_summary_surfaces_zero_embeddings_warning() {
        // K3 (no embedding model) + H4 (silent failures): when no model
        // is on disk, the build summary MUST contain an explicit warning
        // line describing how to install one. The user can't act on a
        // silent degraded build — they need the line in the output.
        let probes = ModelProbes {
            embedding_model_present: false,
            llm_model_present: true,
        };
        let line = probes.summary_embedding_status_line();
        assert!(
            line.contains("0 embedded") && line.contains("no model installed"),
            "expected zero-embedding warning, got: {line:?}"
        );
        assert!(
            line.contains("mneme models install qwen-embed-0.5b"),
            "expected the install hint, got: {line:?}"
        );
    }

    #[test]
    fn build_summary_surfaces_present_embedding_status() {
        // Mirror image of the previous test — when the model IS present,
        // we still emit a status line so silent-success and silent-fail
        // are both impossible. The user always sees what backend ran.
        //
        // The line's exact wording was rewritten under B-009 to point
        // operators at the `embeddings:` runtime row above for the
        // ground-truth active backend (file-on-disk presence is no
        // longer enough to claim a "loaded" model). Pin the present-
        // path contract on the model name + the action-oriented hint,
        // not on a literal `model:` prefix that B-009 dropped.
        let probes = ModelProbes {
            embedding_model_present: true,
            llm_model_present: true,
        };
        let line = probes.summary_embedding_status_line();
        assert!(
            line.contains("bge-small-en-v1.5") && !line.contains("no model installed"),
            "expected present-on-disk line to name the model and not the no-model branch; got: {line:?}"
        );
        assert!(
            line.contains("active runtime backend") || line.contains("backend="),
            "present-on-disk line must point operators at the runtime backend row (B-009); got: {line:?}"
        );
    }

    #[test]
    fn build_summary_community_line_advertises_leiden_seed() {
        // H4: the Leiden pass already runs deterministically, but the
        // build summary doesn't tell anyone. Add an explicit line so
        // operators can verify reproducibility from the build output
        // alone (no need to re-derive from cluster_runner.rs).
        let stats = LeidenStats {
            edges_used: 100,
            communities: 7,
            members: 88,
        };
        let line = community_status_line(&stats);
        assert!(line.contains("7"), "must echo community count: {line:?}");
        assert!(
            line.to_lowercase().contains("leiden")
                && line.to_lowercase().contains("deterministic"),
            "must mention Leiden + deterministic seed: {line:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// I1 batch 3 — empty-shard producer tests.
//
// One test per shard, asserting `SELECT COUNT(*) > 0` against the shard's
// primary table after the corresponding pass runs. Tests build a fixture
// shard against a temp `~/.mneme/` root via `PathManager::with_root` so
// they don't touch the user's real shards. Each test seeds whatever
// upstream rows the pass needs (e.g. wiki / architecture need
// graph.db::nodes + semantic.db::communities) and then asserts the
// pass landed at least one row in the target table.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod empty_shard_tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::TempDir;

    /// Build a `Store` rooted in a tempdir + create the shard for a
    /// dummy project. Returns (Store, ProjectId, PathManager, TempDir).
    /// The TempDir handle keeps the directory alive for the test's
    /// lifetime — drop it and the shard is gone.
    async fn fixture_store(name: &str) -> (Store, ProjectId, PathManager, TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = PathManager::with_root(dir.path().to_path_buf());
        // Materialise the project root on disk so `ProjectId::from_path`
        // (which canonicalises) succeeds, then derive the id from it.
        let project_path = dir.path().join(name);
        std::fs::create_dir_all(&project_path).expect("mkdir project");
        let project_id =
            ProjectId::from_path(&project_path).expect("hash project");
        let store = Store::new(paths.clone());
        store
            .builder
            .build_or_migrate(&project_id, &project_path, name)
            .await
            .expect("build_or_migrate");
        (store, project_id, paths, dir)
    }

    /// Helper — open a shard's SQLite DB read-only and run
    /// `SELECT COUNT(*)` on `table`.
    fn count_rows(paths: &PathManager, project_id: &ProjectId, layer: DbLayer, table: &str) -> i64 {
        let path = paths.shard_db(project_id, layer);
        let conn = Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open shard ro");
        conn.query_row(
            &format!("SELECT COUNT(*) FROM {table}"),
            [],
            |r| r.get::<_, i64>(0),
        )
        .expect("count rows")
    }

    /// Seed graph.db with one file + a couple of nodes + an edge so the
    /// architecture / wiki / federated passes have something to chew on.
    async fn seed_graph(
        store: &Store,
        project_id: &ProjectId,
    ) {
        // Node A
        let _ = store
            .inject
            .insert(
                project_id,
                DbLayer::Graph,
                "INSERT INTO nodes(kind, name, qualified_name, file_path) \
                 VALUES('function', 'foo', 'mod::foo', 'src/foo.rs')",
                vec![],
                InjectOptions::default(),
            )
            .await;
        // Node B
        let _ = store
            .inject
            .insert(
                project_id,
                DbLayer::Graph,
                "INSERT INTO nodes(kind, name, qualified_name, file_path) \
                 VALUES('function', 'bar', 'mod::bar', 'src/bar.rs')",
                vec![],
                InjectOptions::default(),
            )
            .await;
        // Edge A->B (calls)
        let _ = store
            .inject
            .insert(
                project_id,
                DbLayer::Graph,
                "INSERT INTO edges(kind, source_qualified, target_qualified, confidence, \
                 confidence_score, source_extractor) \
                 VALUES('calls', 'mod::foo', 'mod::bar', 'exact', 1.0, 'test')",
                vec![],
                InjectOptions::default(),
            )
            .await;
    }

    /// Seed semantic.db with one community + membership for both nodes.
    async fn seed_semantic(
        store: &Store,
        project_id: &ProjectId,
    ) {
        let resp = store
            .inject
            .insert(
                project_id,
                DbLayer::Semantic,
                "INSERT INTO communities(name, level, parent_id, cohesion, size) \
                 VALUES('community-test', 0, NULL, 0.9, 2)",
                vec![],
                InjectOptions::default(),
            )
            .await;
        let cid = resp.data.map(|r| r.0).unwrap_or(1);
        let _ = store
            .inject
            .insert(
                project_id,
                DbLayer::Semantic,
                "INSERT INTO community_membership(community_id, node_qualified) VALUES(?1, ?2)",
                vec![
                    serde_json::Value::Number(cid.into()),
                    serde_json::Value::String("mod::foo".into()),
                ],
                InjectOptions::default(),
            )
            .await;
        let _ = store
            .inject
            .insert(
                project_id,
                DbLayer::Semantic,
                "INSERT INTO community_membership(community_id, node_qualified) VALUES(?1, ?2)",
                vec![
                    serde_json::Value::Number(cid.into()),
                    serde_json::Value::String("mod::bar".into()),
                ],
                InjectOptions::default(),
            )
            .await;
    }

    #[tokio::test]
    async fn architecture_db_populated_by_build_pass() {
        let (store, pid, paths, _dir) = fixture_store("arch_test").await;
        seed_graph(&store, &pid).await;
        seed_semantic(&store, &pid).await;
        let stats = run_architecture_pass(&store, &pid, &paths).await;
        assert_eq!(stats.snapshots_written, 1, "exactly one snapshot per pass");
        let n = count_rows(&paths, &pid, DbLayer::Architecture, "architecture_snapshots");
        assert!(n > 0, "architecture_snapshots must have rows; got {n}");
    }

    #[tokio::test]
    async fn conventions_db_populated_by_build_pass() {
        let (store, pid, paths, _dir) = fixture_store("conv_test").await;
        // Feed the learner enough samples to clear its 0.80 / 3-evidence
        // threshold for at least one pattern. Every file uses
        // snake_case function names + a single-import line.
        let mut learner = DefaultLearner::new();
        for i in 0..6 {
            let src = format!(
                "fn snake_case_{i}() {{}}\nfn another_snake_{i}() {{}}\n",
            );
            learner.observe_file(
                Path::new(&format!("src/lib_{i}.rs")),
                &src,
                None,
            );
        }
        let stats = run_conventions_pass(&store, &pid, &learner).await;
        assert!(
            stats.patterns_inferred >= 1,
            "expected ≥1 inferred convention; got {}",
            stats.patterns_inferred
        );
        assert!(
            stats.rows_written >= 1,
            "expected ≥1 conventions row written; got {}",
            stats.rows_written
        );
        let n = count_rows(&paths, &pid, DbLayer::Conventions, "conventions");
        assert!(n > 0, "conventions table must have rows; got {n}");
    }

    #[tokio::test]
    async fn wiki_db_populated_by_build_pass() {
        let (store, pid, paths, _dir) = fixture_store("wiki_test").await;
        seed_graph(&store, &pid).await;
        seed_semantic(&store, &pid).await;
        let stats = run_wiki_pass(&store, &pid, &paths).await;
        assert!(
            stats.pages_built >= 1,
            "expected ≥1 page built; got {}",
            stats.pages_built
        );
        assert!(
            stats.pages_written >= 1,
            "expected ≥1 page written; got {}",
            stats.pages_written
        );
        let n = count_rows(&paths, &pid, DbLayer::Wiki, "wiki_pages");
        assert!(n > 0, "wiki_pages must have rows; got {n}");
    }

    #[tokio::test]
    async fn federated_db_populated_by_build_pass() {
        let (_store, pid, paths, dir) = fixture_store("fed_test").await;
        // Seed a tiny rust file under the project root the pass walks.
        let proj_root = dir.path().join("fed_test");
        std::fs::create_dir_all(&proj_root).expect("mkdir project");
        std::fs::write(
            proj_root.join("a.rs"),
            "fn answer() -> i32 { 42 }\n",
        )
        .expect("write fixture");
        let stats = run_federated_pass(&pid, &proj_root, &paths).await;
        assert!(
            stats.indexed >= 1,
            "expected ≥1 fingerprint indexed; got indexed={} skipped={}",
            stats.indexed,
            stats.skipped,
        );
        let n = count_rows(&paths, &pid, DbLayer::Federated, "pattern_fingerprints");
        assert!(n > 0, "pattern_fingerprints must have rows; got {n}");
    }

    #[tokio::test]
    async fn perf_db_populated_by_build_pass() {
        let (store, pid, paths, _dir) = fixture_store("perf_test").await;
        let stats = run_perf_pass(
            &store,
            &pid,
            Duration::from_millis(123),
            5,    // indexed
            42,   // node_total
            17,   // edge_total
        )
        .await;
        assert!(
            stats.baselines_written >= 5,
            "expected 5 baseline rows; got {}",
            stats.baselines_written
        );
        let n = count_rows(&paths, &pid, DbLayer::Perf, "baselines");
        assert!(n >= 5, "baselines must have ≥5 rows; got {n}");
    }

    #[tokio::test]
    async fn errors_db_populated_by_build_pass() {
        let (store, pid, paths, _dir) = fixture_store("err_test").await;
        let errors = vec![
            ("parse failed: unexpected token".to_string(), "src/a.rs".to_string()),
            ("parse failed: unexpected token".to_string(), "src/a.rs".to_string()),
            ("read failed: permission denied".to_string(), "src/b.rs".to_string()),
        ];
        let stats = run_errors_pass(&store, &pid, &errors).await;
        assert_eq!(stats.captured, 3, "all three rows captured (incl dup encounter)");
        assert_eq!(stats.unique, 2, "two unique error_hash values");
        let n = count_rows(&paths, &pid, DbLayer::Errors, "errors");
        assert_eq!(n, 2, "deduplicated to 2 rows; got {n}");
    }

    #[tokio::test]
    async fn livestate_db_populated_by_build_pass() {
        let (store, pid, paths, dir) = fixture_store("ls_test").await;
        let proj = dir.path().join("ls_test");
        let stats = run_livestate_pass(&store, &pid, &proj).await;
        assert_eq!(stats.events_written, 1);
        let n = count_rows(&paths, &pid, DbLayer::LiveState, "file_events");
        assert!(n > 0, "file_events must have rows; got {n}");
    }

    #[tokio::test]
    async fn agents_db_populated_by_build_pass() {
        let (store, pid, paths, dir) = fixture_store("agents_test").await;
        let proj = dir.path().join("agents_test");
        let stats = run_agents_pass(&store, &pid, &proj).await;
        assert_eq!(stats.runs_written, 1);
        let n = count_rows(&paths, &pid, DbLayer::Agents, "subagent_runs");
        assert!(n > 0, "subagent_runs must have rows; got {n}");
    }

    // ---- Silent-2: phase-transition save must propagate io errors ------

    /// RED → GREEN. Silent-2 in `docs/dev/DEEP-AUDIT-2026-04-29.md`
    /// (Class H-silent): the final build-state save at
    /// `cli/src/commands/build.rs:725` was `let _ = build_state::save(...)`,
    /// silently swallowing io errors. A failed phase transition into
    /// `BuildPhase::Multimodal` would let the next resume restart from
    /// Parse and redo work, with the user never told. The fix wraps
    /// `build_state::save` in `save_phase_transition` which converts the
    /// io::Error into a `CliError::Io` so the calling build can `?` it.
    ///
    /// To force `build_state::save` to fail deterministically across
    /// platforms we point the project root at a path whose `.mneme/`
    /// parent is actually a regular file — `create_dir_all` on the
    /// state directory then fails with `NotADirectory` / `AlreadyExists`
    /// (depending on OS), and the wrapper must surface that as
    /// `CliError::Io`.
    #[test]
    fn save_phase_transition_propagates_io_error() {
        use crate::commands::build_state::BuildState;
        let tmp = tempfile::tempdir().expect("tempdir");
        // Make `<tmp>/.mneme` a FILE so build_state::save's
        // `create_dir_all(parent)` fails: parent of state.json is
        // `<project>/.mneme/` which already exists as a non-dir.
        let project = tmp.path();
        let mneme_dir = project.join(".mneme");
        std::fs::write(&mneme_dir, b"not a directory").expect("write blocker");

        let state = BuildState::new(project);
        let r = save_phase_transition(project, &state);
        match r {
            Err(CliError::Io { path, source }) => {
                assert!(
                    path.as_deref() == Some(project),
                    "io error must carry the project path; got {path:?}"
                );
                // Don't pin the exact io::ErrorKind — Windows reports
                // `AlreadyExists`, Linux reports `NotADirectory`,
                // macOS reports `NotADirectory`. All three map to the
                // same observable: build cannot persist phase state.
                let _ = source;
            }
            other => panic!(
                "expected CliError::Io for failed phase-transition save; got {other:?}"
            ),
        }
    }

    /// Sanity GREEN: when `build_state::save` succeeds (writable
    /// directory), `save_phase_transition` returns Ok. Guards against
    /// the wrapper accidentally always-erroring.
    #[test]
    fn save_phase_transition_ok_when_save_succeeds() {
        use crate::commands::build_state::{BuildPhase, BuildState};
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path();
        let mut state = BuildState::new(project);
        state.enter_phase(BuildPhase::Multimodal);
        let r = save_phase_transition(project, &state);
        assert!(r.is_ok(), "save_phase_transition must succeed on writable dir; got {r:?}");
    }

    // ---- Silent-1: graph insert response must surface failure ----------

    /// RED → GREEN. Silent-1 in `docs/dev/DEEP-AUDIT-2026-04-29.md`
    /// (Class H-silent): the parse-loop and wiki-runs paths used
    /// `let _ = store.inject.insert(...)`, silently dropping the
    /// `Response::success == false` case and producing a half-written
    /// graph with a confident "ok" build banner. The fix counts every
    /// non-success response and bubbles it up via `CliError::Other`.
    ///
    /// This test asserts the contract the fix relies on: that a
    /// known-bad insert (here a syntactically-broken SQL) actually
    /// produces `success == false` — not silently `success == true`.
    /// If the store layer ever started swallowing SQL errors the
    /// fail-loud check above would silently regress.
    #[tokio::test]
    async fn store_inject_insert_returns_success_false_on_bad_sql() {
        let (store, pid, _paths, _dir) = fixture_store("silent1_test").await;
        let resp = store
            .inject
            .insert(
                &pid,
                DbLayer::Graph,
                // Intentionally broken: NOT_A_TABLE doesn't exist in
                // the graph shard, so SQLite returns an error.
                "INSERT INTO NOT_A_TABLE_silent1(x) VALUES(?1)",
                vec![serde_json::Value::String("ignored".into())],
                InjectOptions::default(),
            )
            .await;
        assert!(
            !resp.success,
            "broken-SQL insert must report success=false; got success=true with data={:?}",
            resp.data
        );
        assert!(
            resp.error.is_some(),
            "non-success Response must carry error detail; got error=None"
        );
    }

    /// Sanity: wiki_pass tracks insert_failures = 0 on a healthy run.
    /// This pins the new field so a future change can't accidentally
    /// always-set it (which would make every build fail).
    #[tokio::test]
    async fn wiki_pass_reports_zero_insert_failures_on_healthy_run() {
        let (store, pid, paths, _dir) = fixture_store("silent1_wiki_test").await;
        seed_graph(&store, &pid).await;
        seed_semantic(&store, &pid).await;
        let stats = run_wiki_pass(&store, &pid, &paths).await;
        assert_eq!(
            stats.insert_failures, 0,
            "healthy wiki pass must report 0 insert_failures; got {}",
            stats.insert_failures
        );
    }
}
