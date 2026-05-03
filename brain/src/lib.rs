//! mneme — `brain` crate.
//!
//! Local-only intelligence layer for the mneme daemon. Exposes:
//!   * [`embeddings`]   — bge-small-en-v1.5 ONNX embedder
//!   * [`embed_store`]  — disk-backed nearest-neighbour store
//!   * [`leiden`]       — pure-Rust Leiden community detection
//!   * [`concept`]      — deterministic concept extraction (+ optional LLM)
//!   * [`summarize`]    — 1-sentence function summaries
//!   * [`cluster_runner`] — periodic Leiden runner with split policy
//!   * [`worker`]       — async dispatch loop bound to a job channel
//!   * [`job`]          — `BrainJob` / `BrainResult` enums
//!   * [`error`]        — crate error type
//!
//! All subsystems work fully **offline**. If a model file is missing,
//! the corresponding subsystem degrades gracefully (returns empty
//! embeddings, deterministic-only concepts, signature-only summaries).

#![forbid(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]

pub mod blast;
pub mod call_resolver;
pub mod cluster_runner;
pub mod concept;
pub mod conventions;
pub mod embed_store;
pub mod embeddings;
pub mod error;
pub mod federated;
pub mod identity;
pub mod job;
pub mod ledger;
pub mod leiden;
pub mod reranker;
pub mod retrieve;
pub mod summarize;
pub mod wiki;
pub mod worker;

#[cfg(feature = "llm")]
pub mod llm;

#[cfg(test)]
mod tests;

// ---- Local fallback for shared identifiers --------------------------------
//
// The wider workspace is expected to define `common::NodeId` etc. To allow
// this crate to compile in isolation (and to keep the public API stable
// regardless of feature flags) we re-export a thin local definition that
// matches the shape used by the `common` crate.

/// Stable identifier for any node in the mneme graph.
///
/// 128-bit ULID-style identifier serialised as the lower-cased hex of a
/// SHA-256 prefix in degraded mode. Mirrors `common::NodeId`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct NodeId(pub u128);

impl NodeId {
    /// Construct a [`NodeId`] from a raw 128-bit value.
    ///
    /// Callers are responsible for ensuring the bits are derived from a
    /// stable source (e.g. a SHA-256 prefix or ULID); this constructor
    /// performs no validation. `const`-eligible so the type can appear
    /// in `const` contexts.
    pub const fn new(raw: u128) -> Self {
        Self(raw)
    }

    /// Return the underlying 128-bit value.
    ///
    /// Useful for hex display, ULID round-tripping, or
    /// hash-to-bucket computations that don't care about the
    /// `NodeId` newtype wrapper.
    pub fn as_u128(self) -> u128 {
        self.0
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

// ---- Re-exports -----------------------------------------------------------

pub use blast::{build_report, compute_risk, BlastReport, CodeRef, RiskLevel};
pub use call_resolver::{
    build_function_index, extract_callee_name, parse_call_placeholder, resolve_callee,
    CallPlaceholder, IndexedFunction,
};
pub use cluster_runner::ClusterRunner;
pub use concept::{Concept, ConceptExtractor};
pub use conventions::{
    Convention, ConventionLearner, ConventionPattern, DefaultLearner, NamingScope, NamingStyle,
    Violation,
};
pub use embed_store::{EmbedStore, NearestHit};
pub use embeddings::{cosine_similarity, install_default_model, Embedder, EMBEDDING_DIM};
pub use error::{BrainError, BrainResult as BrainOutcome};
pub use federated::{
    minhash_k, simhash_64, FederatedCounts, FederatedStore, PatternFingerprint, MINHASH_K,
    SIMHASH_BITS,
};
pub use identity::{detect_stack, ProjectIdentity, TechCategory, Technology};
pub use job::{BrainJob, BrainResult, JobId};
pub use ledger::{
    new_entry_id, Ledger, LedgerError, LedgerResult, RecallQuery, ResumeBundle, SqliteLedger,
    StepEntry, StepKind, TranscriptRef,
};
pub use leiden::{Community, LeidenConfig, LeidenSolver};
pub use reranker::Reranker;
pub use retrieve::{
    estimate_tokens, BM25Index, GraphIndex, RetrievalEngine, RetrievalResult, RetrievalSource,
    ScoredHit,
};
pub use summarize::Summarizer;
pub use wiki::{CommunityInput, WikiBuilder, WikiPage, WikiSymbol};
pub use worker::{spawn_worker, WorkerHandle};

#[cfg(feature = "llm")]
pub use llm::LocalLlm;
