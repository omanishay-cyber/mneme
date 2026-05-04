//! F2 — Hybrid retrieval engine.
//!
//! Merges three independent retrievers with Reciprocal Rank Fusion (RRF),
//! optionally reranks the top-N with a cross-encoder, and greedily packs
//! the result into a token budget.
//!
//! Retrievers:
//!   1. **BM25** — pure-Rust lexical scoring. Uses a bounded in-memory
//!      index built from `(id, text)` pairs the caller supplies. We do NOT
//!      take a hard dependency on `tantivy` here: the Rust toolchain on
//!      some tier-1 platforms has trouble with its OpenSSL feature, and
//!      this crate already ships a deterministic hashing-trick backend we
//!      want to keep compilation-clean. A drop-in `tantivy` variant can
//!      be swapped in later without touching the `RetrievalEngine` API.
//!   2. **Semantic** — reuses [`crate::embed_store::EmbedStore`] so the
//!      BGE-small vectors already on disk power this path for free.
//!   3. **Graph** — two-hop neighborhood expansion via [`petgraph`].
//!
//! Fusion is Reciprocal Rank Fusion with `k = 60` — the canonical
//! constant from Cormack et al. (2009). Rerank is optional; when a
//! [`crate::reranker::Reranker`] is installed the top `RERANK_CANDIDATES`
//! hits are rescored before packing.
//!
//! The greedy packer orders hits by their fused score and adds them to the
//! context pack until the cumulative estimated token count would exceed
//! `budget_tokens`. No partial-hit truncation: a hit is either in or out.

use std::collections::HashMap;
use std::sync::Arc;

use petgraph::graph::NodeIndex;
use petgraph::Graph;
use serde::{Deserialize, Serialize};

use crate::embed_store::EmbedStore;
use crate::embeddings::Embedder;
use crate::error::BrainResult;
#[cfg(feature = "reranker")]
use crate::reranker::Reranker;
use crate::NodeId;

/// Reciprocal Rank Fusion constant. Cormack et al. 2009.
const RRF_K: f32 = 60.0;

/// How many fused hits to hand to the reranker. Keeps latency bounded.
const RERANK_CANDIDATES: usize = 25;

/// Rough chars-per-token estimate used by the greedy packer. Matches
/// `mcp/src/composer.ts`.
const CHARS_PER_TOKEN: usize = 4;

/// One retrieval candidate — opaque id + display text + provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoredHit {
    /// Opaque id (e.g. qualified symbol name, file path, step_id).
    pub id: String,
    /// Text blob that will be counted against the token budget.
    pub text: String,
    /// Fused score in `[0, 1]` — higher is better.
    pub score: f32,
    /// Which retriever(s) surfaced this hit.
    pub sources: Vec<RetrievalSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RetrievalSource {
    Bm25,
    Semantic,
    Graph,
    Reranker,
}

/// Result of [`RetrievalEngine::retrieve`].
///
/// BUG-A2-015 fix: `semantic_available` lets the caller (and `mneme_recall`
/// MCP tool) surface a clear "running BM25+Graph only" message instead of
/// silently degrading retrieval quality.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalResult {
    pub hits: Vec<ScoredHit>,
    pub tokens_used: u32,
    pub budget_tokens: u32,
    pub latency_ms: u64,
    /// True iff the embedder was installed AND used during this retrieval.
    /// When false, the caller should warn the user that semantic search
    /// is offline (likely missing BGE model).
    #[serde(default = "default_true")]
    pub semantic_available: bool,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// BM25 — minimal pure-Rust implementation
// ---------------------------------------------------------------------------

/// Bounded in-memory BM25 index. `build_from_documents` is the single
/// entry point; callers rebuild when their corpus changes. The index is
/// intentionally simple: we maintain term-frequency and document-length
/// tables and score with the standard BM25 formula (`k1 = 1.5`, `b = 0.75`).
#[derive(Debug, Default)]
pub struct BM25Index {
    /// Per-document token multiset + total token count.
    docs: Vec<DocEntry>,
    /// Inverse document frequency per term.
    idf: HashMap<String, f32>,
    /// Average document length across the corpus.
    avg_len: f32,
    /// Display text for each doc (what we return through [`ScoredHit::text`]).
    texts: Vec<String>,
    /// Opaque ids for each doc.
    ids: Vec<String>,
}

#[derive(Debug, Default)]
struct DocEntry {
    terms: HashMap<String, u32>,
    len: u32,
}

impl BM25Index {
    /// Build from `(id, text)` pairs. Tokenisation is whitespace + lowercase;
    /// this matches the fallback embedder so both paths agree on the token
    /// set.
    pub fn build_from_documents(docs: Vec<(String, String)>) -> Self {
        let mut idx = BM25Index::default();
        let mut df: HashMap<String, u32> = HashMap::new();
        let mut total_len: u64 = 0;

        for (id, text) in docs.into_iter() {
            let tokens = tokenize(&text);
            let mut entry = DocEntry::default();
            for t in &tokens {
                *entry.terms.entry(t.clone()).or_insert(0) += 1;
            }
            entry.len = tokens.len() as u32;
            total_len += entry.len as u64;
            for t in entry.terms.keys() {
                *df.entry(t.clone()).or_insert(0) += 1;
            }
            idx.texts.push(text);
            idx.ids.push(id);
            idx.docs.push(entry);
        }

        let n = idx.docs.len().max(1) as f32;
        for (t, d) in df {
            let val = ((n - d as f32 + 0.5) / (d as f32 + 0.5) + 1.0).ln();
            idx.idf.insert(t, val);
        }
        idx.avg_len = if idx.docs.is_empty() {
            0.0
        } else {
            total_len as f32 / idx.docs.len() as f32
        };
        idx
    }

    /// BM25 score for a free-form query. Returns `(doc_id, score)` pairs
    /// sorted by descending score. Docs with a zero score are omitted.
    pub fn search(&self, query: &str, top_k: usize) -> Vec<(String, f32)> {
        if self.docs.is_empty() || top_k == 0 {
            return Vec::new();
        }
        const K1: f32 = 1.5;
        const B: f32 = 0.75;
        let q_tokens = tokenize(query);
        let mut scored: Vec<(usize, f32)> = Vec::with_capacity(self.docs.len());
        for (i, doc) in self.docs.iter().enumerate() {
            let mut s = 0f32;
            for t in &q_tokens {
                let tf = *doc.terms.get(t).unwrap_or(&0) as f32;
                if tf == 0.0 {
                    continue;
                }
                let idf = *self.idf.get(t).unwrap_or(&0.0);
                let denom = tf + K1 * (1.0 - B + B * (doc.len as f32 / self.avg_len.max(1.0)));
                s += idf * ((tf * (K1 + 1.0)) / denom.max(f32::EPSILON));
            }
            if s > 0.0 {
                scored.push((i, s));
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored
            .into_iter()
            .take(top_k)
            .map(|(i, s)| (self.ids[i].clone(), s))
            .collect()
    }

    /// Resolve an id to its display text (used by the packer).
    pub fn text_for(&self, id: &str) -> Option<&str> {
        self.ids
            .iter()
            .position(|x| x == id)
            .map(|i| self.texts[i].as_str())
    }
}

fn tokenize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.len() >= 2)
        .map(|t| t.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Graph — opaque wrapper around petgraph for 2-hop neighbourhood expansion
// ---------------------------------------------------------------------------

/// Lightweight graph index. Node labels are opaque strings; edges are
/// unweighted. Callers build the graph once and hand it to the engine.
#[derive(Debug, Default)]
pub struct GraphIndex {
    inner: Graph<String, ()>,
    by_label: HashMap<String, NodeIndex>,
    /// Display text for each node (fallback = label).
    text: HashMap<String, String>,
}

impl GraphIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_node(&mut self, label: &str, text: &str) -> NodeIndex {
        if let Some(&ix) = self.by_label.get(label) {
            return ix;
        }
        let ix = self.inner.add_node(label.to_string());
        self.by_label.insert(label.to_string(), ix);
        self.text.insert(label.to_string(), text.to_string());
        ix
    }

    pub fn add_edge(&mut self, from: &str, to: &str) {
        let a = self.add_node(from, from);
        let b = self.add_node(to, to);
        self.inner.add_edge(a, b, ());
    }

    /// 2-hop neighborhood around `anchors`. Returns `(label, depth)` pairs.
    pub fn two_hop(&self, anchors: &[String]) -> Vec<(String, u32)> {
        let mut out: HashMap<String, u32> = HashMap::new();
        for a in anchors {
            let Some(&start) = self.by_label.get(a) else {
                continue;
            };
            out.entry(a.clone()).or_insert(0);
            for n1 in self.inner.neighbors(start) {
                let l1 = self.inner[n1].clone();
                out.entry(l1).and_modify(|d| *d = (*d).min(1)).or_insert(1);
                for n2 in self.inner.neighbors(n1) {
                    let l2 = self.inner[n2].clone();
                    out.entry(l2).and_modify(|d| *d = (*d).min(2)).or_insert(2);
                }
            }
        }
        let mut v: Vec<(String, u32)> = out.into_iter().collect();
        v.sort_by_key(|(_, d)| *d);
        v
    }

    pub fn text_for(&self, label: &str) -> Option<&str> {
        self.text.get(label).map(|s| s.as_str())
    }
}

// ---------------------------------------------------------------------------
// Retrieval engine
// ---------------------------------------------------------------------------

/// Top-level hybrid retrieval engine.
///
/// Wrapped fields are public so the supervisor can bolt indexes on
/// incrementally (BM25 first, graph later, reranker last of all).
pub struct RetrievalEngine {
    pub bm25: BM25Index,
    pub embed: Arc<EmbedStore>,
    pub graph: GraphIndex,
    /// Optional cross-encoder reranker. Feature-gated at compile time.
    #[cfg(feature = "reranker")]
    pub reranker: Option<Reranker>,
    /// Embedder used for semantic search. Optional — if absent the semantic
    /// retriever returns empty (graceful degradation for fresh installs).
    pub embedder: Option<Embedder>,
    /// Mapping from opaque node_id (u128) → display text, populated at
    /// index-build time. The embed store only stores ids + vectors.
    pub node_text: HashMap<NodeId, String>,
    /// Mapping from opaque node_id → caller-supplied string id, so the
    /// fusion step can unify results across retrievers.
    pub node_labels: HashMap<NodeId, String>,
    /// BUG-A2-016 fix: reverse index `label -> node_id` so `text_for` is
    /// O(1) instead of O(N) per semantic-only hit. Populated lazily on
    /// first lookup; safe because both maps are append-only after build.
    pub label_to_node: HashMap<String, NodeId>,
}

impl std::fmt::Debug for RetrievalEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetrievalEngine")
            .field("bm25_docs", &self.bm25.docs.len())
            .field("embed_count", &self.embed.len())
            .field("graph_nodes", &self.graph.by_label.len())
            .field("has_embedder", &self.embedder.is_some())
            .finish()
    }
}

impl RetrievalEngine {
    pub fn new(embed: Arc<EmbedStore>) -> Self {
        Self {
            bm25: BM25Index::default(),
            embed,
            graph: GraphIndex::new(),
            #[cfg(feature = "reranker")]
            reranker: None,
            embedder: None,
            node_text: HashMap::new(),
            node_labels: HashMap::new(),
            label_to_node: HashMap::new(),
        }
    }

    /// Re-derive the reverse `label -> node_id` map from `node_labels`.
    /// Call after the engine's labels are populated; the retrieve path
    /// will lazy-rebuild if it's empty but stale rebuilds are free.
    /// BUG-A2-016 helper.
    pub fn rebuild_label_index(&mut self) {
        self.label_to_node = self
            .node_labels
            .iter()
            .map(|(nid, lbl)| (lbl.clone(), *nid))
            .collect();
    }

    /// Install an embedder (optional).
    pub fn with_embedder(mut self, e: Embedder) -> Self {
        self.embedder = Some(e);
        self
    }

    /// Install a reranker (optional, feature-gated).
    #[cfg(feature = "reranker")]
    pub fn with_reranker(mut self, r: Reranker) -> Self {
        self.reranker = Some(r);
        self
    }

    /// Retrieve a context pack for `task`, optionally anchored to known
    /// labels (which seeds the graph-walk path).
    pub fn retrieve(
        &self,
        task: &str,
        budget_tokens: u32,
        anchors: &[String],
    ) -> BrainResult<RetrievalResult> {
        let t0 = std::time::Instant::now();

        // BUG-A2-015 fix: capture semantic-availability for the result.
        let semantic_available = self.embedder.is_some();

        // 1. Parallel retrievers — in-process, no async needed.
        let bm25_hits = self.bm25.search(task, 50);
        let semantic_hits = self.semantic_search(task, 50)?;
        let graph_hits = self.graph.two_hop(anchors);

        // 2. Reciprocal rank fusion.
        let mut fused: HashMap<String, (f32, Vec<RetrievalSource>)> = HashMap::new();

        for (rank, (id, _)) in bm25_hits.iter().enumerate() {
            let entry = fused.entry(id.clone()).or_insert_with(|| (0.0, Vec::new()));
            entry.0 += 1.0 / (RRF_K + rank as f32 + 1.0);
            entry.1.push(RetrievalSource::Bm25);
        }
        for (rank, (id, _)) in semantic_hits.iter().enumerate() {
            let entry = fused.entry(id.clone()).or_insert_with(|| (0.0, Vec::new()));
            entry.0 += 1.0 / (RRF_K + rank as f32 + 1.0);
            entry.1.push(RetrievalSource::Semantic);
        }
        for (rank, (id, _)) in graph_hits.iter().enumerate() {
            let entry = fused.entry(id.clone()).or_insert_with(|| (0.0, Vec::new()));
            // Graph walks get half weight — they represent structural
            // priors, not direct relevance.
            entry.0 += 0.5 / (RRF_K + rank as f32 + 1.0);
            entry.1.push(RetrievalSource::Graph);
        }

        let mut candidates: Vec<(String, f32, Vec<RetrievalSource>)> = fused
            .into_iter()
            .map(|(id, (score, sources))| (id, score, sources))
            .collect();
        candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // 3. Optional reranker — top N only.
        // BUG-A2-033 fix: skip the rerank step entirely when the inner
        // backend is a stub. Pre-fix the engine still tagged the top N
        // hits with `RetrievalSource::Reranker`, lying about a transform
        // that didn't happen.
        #[cfg(feature = "reranker")]
        if let Some(rr) = self.reranker.as_ref().filter(|r| r.is_active()) {
            let top: Vec<(String, f32)> = candidates
                .iter()
                .take(RERANK_CANDIDATES)
                .map(|(id, s, _)| {
                    let txt = self.text_for(id).unwrap_or_else(|| id.clone());
                    (txt, *s)
                })
                .collect();
            if let Ok(rescored) = rr.rerank(task, top) {
                // BUG-A2-017 fix: blend the rerank output with the existing
                // RRF score after min-max normalising the rerank scores
                // into `[0, 1]`. Pre-fix, raw cross-encoder logits could be
                // negative or unbounded and clobbered the RRF scale, which
                // made positions 26+ (still on RRF scale) incorrectly
                // outrank reranked items at position N for negative logits.
                let rescored: Vec<(String, f32)> = rescored;
                let (mut min_s, mut max_s) = (f32::INFINITY, f32::NEG_INFINITY);
                for (_, s) in &rescored {
                    if *s < min_s {
                        min_s = *s;
                    }
                    if *s > max_s {
                        max_s = *s;
                    }
                }
                let span = (max_s - min_s).max(f32::EPSILON);
                for (i, (_text, new_score)) in rescored.into_iter().enumerate() {
                    if let Some(c) = candidates.get_mut(i) {
                        let normalised = ((new_score - min_s) / span).clamp(0.0, 1.0);
                        // 50/50 blend keeps RRF agreement when the
                        // reranker is weak / stub-like and lets a
                        // confident reranker win the tie-break.
                        c.1 = 0.5 * c.1 + 0.5 * normalised;
                        c.2.push(RetrievalSource::Reranker);
                    }
                }
                candidates
                    .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            }
        }

        // Silence the unused-variable warning when feature is off.
        let _ = RERANK_CANDIDATES;

        // 4. Greedy pack into the token budget.
        let mut out = Vec::new();
        let mut used: u32 = 0;
        for (id, score, sources) in candidates {
            let text = self.text_for(&id).unwrap_or_else(|| id.clone());
            let est = estimate_tokens(&text);
            if used + est > budget_tokens {
                continue;
            }
            out.push(ScoredHit {
                id,
                text,
                score: score.clamp(0.0, 1.0),
                sources,
            });
            used += est;
            if used >= budget_tokens {
                break;
            }
        }

        Ok(RetrievalResult {
            hits: out,
            tokens_used: used,
            budget_tokens,
            latency_ms: t0.elapsed().as_millis() as u64,
            semantic_available,
        })
    }

    /// Semantic search via the shared [`EmbedStore`]. Returns `(label, score)`
    /// pairs so the RRF step has a uniform key across retrievers.
    fn semantic_search(&self, query: &str, top_k: usize) -> BrainResult<Vec<(String, f32)>> {
        let Some(embedder) = self.embedder.as_ref() else {
            return Ok(Vec::new());
        };
        let vector = embedder.embed(query)?;
        let hits = self.embed.nearest(&vector, top_k);
        let out = hits
            .into_iter()
            .filter_map(|h| {
                self.node_labels
                    .get(&h.node)
                    .cloned()
                    .map(|lbl| (lbl, h.score))
            })
            .collect();
        Ok(out)
    }

    fn text_for(&self, id: &str) -> Option<String> {
        if let Some(t) = self.bm25.text_for(id) {
            return Some(t.to_string());
        }
        if let Some(t) = self.graph.text_for(id) {
            return Some(t.to_string());
        }
        // Semantic-only hit: look up via the reverse `label -> node_id` map.
        // BUG-A2-016 fix: O(1) hash lookup instead of an O(N) linear scan.
        // When the reverse map hasn't been populated yet (the legacy
        // build path didn't call `rebuild_label_index`), fall back to the
        // O(N) scan so the call still succeeds — slow but correct.
        if let Some(nid) = self.label_to_node.get(id) {
            return self.node_text.get(nid).cloned();
        }
        self.node_labels
            .iter()
            .find(|(_, lbl)| lbl.as_str() == id)
            .and_then(|(nid, _)| self.node_text.get(nid).cloned())
    }
}

/// chars/4 token estimator — matches `mcp/src/composer.ts`.
pub fn estimate_tokens(s: &str) -> u32 {
    if s.is_empty() {
        0
    } else {
        s.len().div_ceil(CHARS_PER_TOKEN) as u32
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bm25_finds_exact_token() {
        let idx = BM25Index::build_from_documents(vec![
            ("a".to_string(), "the quick brown fox".to_string()),
            ("b".to_string(), "lazy dog sleeps all day".to_string()),
            ("c".to_string(), "quick reflexes win races".to_string()),
        ]);
        let hits = idx.search("quick", 10);
        let ids: Vec<_> = hits.into_iter().map(|(id, _)| id).collect();
        assert!(ids.contains(&"a".to_string()));
        assert!(ids.contains(&"c".to_string()));
    }

    #[test]
    fn graph_two_hop_finds_neighbors() {
        let mut g = GraphIndex::new();
        g.add_edge("a", "b");
        g.add_edge("b", "c");
        g.add_edge("c", "d");
        let hits = g.two_hop(&["a".to_string()]);
        let labels: Vec<_> = hits.iter().map(|(l, _)| l.as_str()).collect();
        assert!(labels.contains(&"a"));
        assert!(labels.contains(&"b"));
        assert!(labels.contains(&"c"));
        // d is 3 hops away — excluded.
        assert!(!labels.contains(&"d"));
    }

    #[test]
    fn engine_packs_within_budget() {
        let store =
            Arc::new(EmbedStore::open(&std::env::temp_dir().join("mneme-ret-test")).unwrap());
        let mut eng = RetrievalEngine::new(store);
        eng.bm25 = BM25Index::build_from_documents(vec![
            ("d1".to_string(), "alpha beta gamma".to_string()),
            ("d2".to_string(), "beta delta epsilon".to_string()),
        ]);
        let out = eng.retrieve("beta", 100, &[]).unwrap();
        assert!(out.tokens_used <= out.budget_tokens);
        assert!(!out.hits.is_empty());
    }

    #[test]
    fn token_estimator_matches_mcp_composer() {
        // 16 chars → 4 tokens (chars/4 rounded up)
        assert_eq!(estimate_tokens("0123456789abcdef"), 4);
        assert_eq!(estimate_tokens("0123456789abcdefg"), 5);
        assert_eq!(estimate_tokens(""), 0);
    }
}
