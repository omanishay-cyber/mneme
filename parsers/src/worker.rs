//! Async worker loop — drains [`ParseJob`]s from an MPSC and sends back
//! [`ParseResult`]s.
//!
//! Per design §3.4 + §21.3:
//! - Workers communicate over MPSC, never shared state.
//! - One [`tree_sitter::Parser`] per (language, worker) — provided by the
//!   shared [`ParserPool`].
//! - Edits arrive pre-debounced (50ms upstream — the file watcher in the
//!   `scanners` crate batches keystrokes into one `ParseJob`).

use crate::error::ParserError;
use crate::extractor::Extractor;
use crate::incremental::IncrementalParser;
use crate::job::{ParseJob, ParseResult};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{debug, error, info, instrument, warn};

/// One parser worker. Multiple workers share a single [`IncrementalParser`]
/// (which itself shares a single [`crate::ParserPool`]); each worker holds
/// its own MPSC receiver so the supervisor can fan out jobs round-robin.
pub struct Worker {
    /// Stable, human-readable id for tracing & logs.
    pub id: usize,
    incremental: Arc<IncrementalParser>,
    rx: mpsc::Receiver<ParseJob>,
    tx_results: mpsc::Sender<Result<ParseResult, ParserError>>,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker").field("id", &self.id).finish()
    }
}

impl Worker {
    /// Construct a worker. The caller wires up the channels.
    pub fn new(
        id: usize,
        incremental: Arc<IncrementalParser>,
        rx: mpsc::Receiver<ParseJob>,
        tx_results: mpsc::Sender<Result<ParseResult, ParserError>>,
    ) -> Self {
        Self {
            id,
            incremental,
            rx,
            tx_results,
        }
    }

    /// Run the loop until the job channel closes.
    ///
    /// Cancellation: dropping the `Sender` end of `rx` breaks the loop
    /// cleanly. We never panic on parse failures — they flow back to the
    /// supervisor as `Err(ParserError)` for restart accounting.
    #[instrument(skip(self), fields(worker_id = self.id))]
    pub async fn run(mut self) {
        info!(worker = self.id, "parser worker online");
        while let Some(job) = self.rx.recv().await {
            let result = self.handle(job).await;
            if let Err(send_err) = self.tx_results.send(result).await {
                warn!(worker = self.id, error = %send_err, "result channel closed; shutting down worker");
                break;
            }
        }
        info!(worker = self.id, "parser worker offline");
    }

    async fn handle(&self, job: ParseJob) -> Result<ParseResult, ParserError> {
        let started = Instant::now();
        debug!(
            worker = self.id,
            file = %job.file_path.display(),
            language = %job.language,
            job_id = job.job_id,
            "handling parse job"
        );

        // A3-017 (2026-05-04): per-file timeout. A pathological grammar
        // input (deeply nested generic, ambiguous JSX, corrupted source
        // that confuses the parser) can cause `parse_file` to take many
        // minutes -- without this guard, a single bad file wedges the
        // worker forever and the watchdog cannot catch it (no heartbeat
        // emission per A4-003). 60 s is generous for any realistic
        // file; pathological inputs trip the timeout and the supervisor
        // sees a clean ParseFailed error to dispatch the next job.
        const PARSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

        // 1. Parse (incremental if a previous tree exists).
        let parse = match tokio::time::timeout(
            PARSE_TIMEOUT,
            self.incremental
                .parse_file(&job.file_path, job.language, job.content.clone()),
        )
        .await
        {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                error!(worker = self.id, file = %job.file_path.display(), error = %e, "parse failed");
                return Err(e);
            }
            Err(_elapsed) => {
                error!(
                    worker = self.id,
                    file = %job.file_path.display(),
                    timeout_secs = PARSE_TIMEOUT.as_secs(),
                    "parse exceeded timeout -- pathological input or grammar wedge"
                );
                return Err(ParserError::ParseFailed(job.file_path.clone()));
            }
        };

        // 2. Short-circuit when the file is byte-identical — no extraction
        //    needed; the brain crate keeps its cached graph.
        if parse.unchanged {
            return Ok(ParseResult::unchanged(&job));
        }

        // 3. Extract nodes/edges.
        let extractor = Extractor::new(job.language);
        let extracted = extractor.extract(&parse.tree, &job.content, &job.file_path)?;

        let dur = started.elapsed();
        debug!(
            worker = self.id,
            file = %job.file_path.display(),
            duration_ms = dur.as_millis() as u64,
            nodes = extracted.nodes.len(),
            edges = extracted.edges.len(),
            issues = extracted.issues.len(),
            incremental = parse.incremental,
            "parse complete"
        );

        Ok(ParseResult {
            job_id: job.job_id,
            file_path: job.file_path,
            language: job.language,
            nodes: extracted.nodes,
            edges: extracted.edges,
            syntax_errors: extracted.issues,
            parse_duration_ms: dur.as_millis() as u64,
            incremental: parse.incremental,
        })
    }
}
