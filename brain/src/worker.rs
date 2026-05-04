//! Async worker that owns the embedder, store, extractor and runners and
//! dispatches incoming [`BrainJob`]s onto them.
//!
//! Architecture:
//! ```text
//!   caller --(BrainJob via mpsc)--> worker --(BrainResult via mpsc)--> caller
//! ```
//!
//! The worker is single-threaded with respect to the underlying ONNX session
//! (which is itself not `Sync`), but it spawns blocking work onto Tokio's
//! blocking pool so the runtime stays responsive.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::cluster_runner::{ClusterRunner, ClusterRunnerConfig};
use crate::concept::{ConceptExtractor, ExtractInput};
use crate::embed_store::EmbedStore;
use crate::embeddings::Embedder;
use crate::error::BrainResult;
use crate::job::{BrainJob, BrainResult as JobResult};
use crate::summarize::Summarizer;

/// Public worker handle returned from [`spawn_worker`].
#[derive(Debug)]
pub struct WorkerHandle {
    pub jobs_tx: mpsc::Sender<BrainJob>,
    pub results_rx: mpsc::Receiver<JobResult>,
    pub join: JoinHandle<()>,
}

/// Construction-time options.
#[derive(Clone)]
pub struct WorkerConfig {
    pub embedder: Embedder,
    pub store: EmbedStore,
    pub extractor: ConceptExtractor,
    pub summarizer: Summarizer,
    pub cluster: ClusterRunner,
    pub channel_capacity: usize,
    /// BUG-A2-043 fix: flush the embed store every N seconds during normal
    /// operation, so a daemon crash doesn't lose all writes since the last
    /// successful flush. Default = 60s; 0 disables.
    pub flush_interval_secs: u64,
    /// BUG-A2-044 fix: per-job timeout. Slow stdout consumer + fast job
    /// producer used to balloon job latency unboundedly because there was
    /// no per-job deadline. Default = 5 minutes; 0 disables.
    pub job_timeout_secs: u64,
}

impl std::fmt::Debug for WorkerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerConfig")
            .field("channel_capacity", &self.channel_capacity)
            .finish()
    }
}

impl WorkerConfig {
    /// Build with all-default subsystems. Fails only if `EmbedStore` cannot
    /// open its cache directory.
    pub fn with_defaults() -> BrainResult<Self> {
        Ok(Self {
            embedder: Embedder::from_default_path()?,
            store: EmbedStore::open_default()?,
            extractor: ConceptExtractor::new(),
            summarizer: Summarizer::new(),
            cluster: ClusterRunner::new(ClusterRunnerConfig::default()),
            channel_capacity: 256,
            flush_interval_secs: 60,
            job_timeout_secs: 300,
        })
    }
}

/// Spawn the worker. Returns sender/receiver pair plus the join handle.
pub fn spawn_worker(cfg: WorkerConfig) -> WorkerHandle {
    let (jobs_tx, mut jobs_rx) = mpsc::channel::<BrainJob>(cfg.channel_capacity);
    let (results_tx, results_rx) = mpsc::channel::<JobResult>(cfg.channel_capacity);

    let embedder = cfg.embedder.clone();
    let store = cfg.store.clone();
    let extractor = cfg.extractor.clone();
    let summarizer = cfg.summarizer.clone();
    let cluster = cfg.cluster.clone();
    let flush_interval = cfg.flush_interval_secs;
    let job_timeout = cfg.job_timeout_secs;

    let join = tokio::spawn(async move {
        info!("brain worker started");
        // BUG-A2-043 fix: periodic flush so unflushed embeds are bounded
        // even if the daemon crashes between explicit flushes.
        let mut flush_tick = if flush_interval > 0 {
            Some(tokio::time::interval(Duration::from_secs(flush_interval)))
        } else {
            None
        };
        // Skip the immediate first tick (intervals fire at zero by default).
        if let Some(t) = flush_tick.as_mut() {
            t.tick().await;
        }
        loop {
            tokio::select! {
                maybe_job = jobs_rx.recv() => {
                    let Some(job) = maybe_job else { break; };
                    if matches!(job, BrainJob::Shutdown) {
                        info!("brain worker shutting down");
                        break;
                    }
                    // BUG-A2-044 fix: cap each job at `job_timeout_secs` so
                    // backpressure can't balloon individual job latency.
                    let job_id = job.id();
                    let fut = handle(
                        job,
                        embedder.clone(),
                        store.clone(),
                        extractor.clone(),
                        summarizer.clone(),
                        cluster.clone(),
                    );
                    let result = if job_timeout > 0 {
                        match tokio::time::timeout(Duration::from_secs(job_timeout), fut).await {
                            Ok(r) => r,
                            Err(_) => {
                                warn!(job_id, secs = job_timeout, "brain job timed out");
                                JobResult::Error {
                                    id: job_id,
                                    message: format!("job timed out after {}s", job_timeout),
                                }
                            }
                        }
                    } else {
                        fut.await
                    };
                    if results_tx.send(result).await.is_err() {
                        warn!("brain result receiver dropped - exiting");
                        break;
                    }
                }
                _ = async {
                    if let Some(t) = flush_tick.as_mut() {
                        t.tick().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                }, if flush_tick.is_some() => {
                    if let Err(e) = store.flush() {
                        warn!(error = %e, "periodic embed store flush failed");
                    }
                }
            }
        }
        // BUG-A2-043 fix: on shutdown flush failure, write to a
        // recovery file so the next startup can pick up unflushed data.
        if let Err(e) = store.flush() {
            warn!(error = %e, "embed store flush on shutdown failed");
            // Try to write a marker so the next startup can warn the user.
            if let Some(dir) = dirs::home_dir() {
                let marker = dir.join(".mneme").join("cache").join("embed").join(
                    format!("pending_{}.marker", chrono::Utc::now().timestamp()),
                );
                if let Some(parent) = marker.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(me) = std::fs::write(&marker, format!("flush failed: {e}\n")) {
                    warn!(error = %me, path = %marker.display(), "failed to write pending-flush marker");
                }
            }
        }
    });

    WorkerHandle {
        jobs_tx,
        results_rx,
        join,
    }
}

async fn handle(
    job: BrainJob,
    embedder: Embedder,
    store: EmbedStore,
    extractor: ConceptExtractor,
    summarizer: Summarizer,
    cluster: ClusterRunner,
) -> JobResult {
    let id = job.id();
    match job {
        BrainJob::Embed { id, node, text } => {
            let res = run_blocking(move || {
                let v = embedder.embed(&text)?;
                if let Some(n) = node {
                    // BUG-A2-042 fix: invalidate the embedder cache entry
                    // when the store upsert fails so a future
                    // `embedder.embed(&text)` doesn't return a vector
                    // that the store doesn't actually have.
                    if let Err(e) = store.upsert(n, &v) {
                        embedder.invalidate_cache_for(&text);
                        return Err(e);
                    }
                }
                Ok::<_, crate::error::BrainError>(v)
            })
            .await;
            match res {
                Ok(vector) => JobResult::Embedding { id, node, vector },
                Err(e) => JobResult::Error {
                    id,
                    message: e.to_string(),
                },
            }
        }
        BrainJob::EmbedBatch { id, items } => {
            let res = run_blocking(move || {
                let texts: Vec<&str> = items.iter().map(|(_, t)| t.as_str()).collect();
                let vectors = embedder.embed_batch(&texts)?;
                let mut out = Vec::with_capacity(items.len());
                let mut to_store: Vec<(crate::NodeId, Vec<f32>)> = Vec::new();
                for ((node, _), v) in items.iter().zip(vectors) {
                    if let Some(n) = node {
                        to_store.push((*n, v.clone()));
                    }
                    out.push((*node, v));
                }
                if !to_store.is_empty() {
                    if let Err(e) = store.upsert_many(&to_store) {
                        // BUG-A2-042 fix: invalidate the cache for every
                        // text in the failed batch so the embedder cache
                        // and persisted store can't drift.
                        for (_, t) in &items {
                            embedder.invalidate_cache_for(t);
                        }
                        return Err(e);
                    }
                }
                Ok::<_, crate::error::BrainError>(out)
            })
            .await;
            match res {
                Ok(vectors) => JobResult::EmbeddingBatch { id, vectors },
                Err(e) => JobResult::Error {
                    id,
                    message: e.to_string(),
                },
            }
        }
        BrainJob::Cluster { id, edges, seed } => {
            let mut local_cluster = cluster.clone();
            // Override seed if caller supplied one.
            if let Some(s) = seed {
                let mut cfg = ClusterRunnerConfig::default();
                cfg.leiden.seed = s;
                local_cluster = ClusterRunner::new(cfg);
            }
            let res = run_blocking(move || local_cluster.run(&edges)).await;
            match res {
                Ok(communities) => JobResult::Clusters { id, communities },
                Err(e) => JobResult::Error {
                    id,
                    message: e.to_string(),
                },
            }
        }
        BrainJob::ExtractConcepts {
            id,
            node,
            kind,
            text,
        } => {
            let res = run_blocking(move || {
                extractor.extract(ExtractInput {
                    kind: &kind,
                    text: &text,
                })
            })
            .await;
            match res {
                Ok(concepts) => JobResult::Concepts { id, node, concepts },
                Err(e) => JobResult::Error {
                    id,
                    message: e.to_string(),
                },
            }
        }
        BrainJob::Summarize {
            id,
            node,
            signature,
            body,
        } => {
            let res = run_blocking(move || summarizer.summarize_function(&signature, &body)).await;
            match res {
                Ok(summary) => JobResult::Summary { id, node, summary },
                Err(e) => JobResult::Error {
                    id,
                    message: e.to_string(),
                },
            }
        }
        BrainJob::Shutdown => JobResult::Error {
            id,
            message: "shutdown is not a job".into(),
        },
    }
}

/// Convenience: run CPU-bound work on the blocking pool, propagating panics
/// as errors instead of crashing the worker.
async fn run_blocking<F, T>(f: F) -> Result<T, crate::error::BrainError>
where
    F: FnOnce() -> Result<T, crate::error::BrainError> + Send + 'static,
    T: Send + 'static,
{
    let _ = Arc::new(()); // anchor for clippy: fn signature stable across builds
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "blocking task panicked");
            Err(crate::error::BrainError::Other(anyhow::anyhow!(
                "blocking task panicked: {e}"
            )))
        }
    }
}
