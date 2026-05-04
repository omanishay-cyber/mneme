//! `ArchitectureScanner` — coupling / risk / bridge analysis.
//!
//! Unlike the other file-level scanners under this module, the architecture
//! overview is a graph-level analysis: it operates on a pre-built node+edge
//! set (typically pulled from `graph.db` and `semantic.db`) and produces a
//! single summary structure. It therefore does not implement the
//! file-oriented [`crate::scanner::Scanner`] trait; instead it exposes
//! [`ArchitectureScanner::analyze`] which the MCP `architecture_overview`
//! tool (and its Rust supervisor plumbing) drives directly.
//!
//! Outputs:
//!
//! * `coupling_matrix` — dense N_community x N_community matrix of edge
//!   density between communities; diagonal is intra-community density.
//! * `risk_index`      — per-community score = callers x criticality x
//!   security_flag, sorted descending.
//! * `bridge_nodes`    — top-K nodes by betweenness centrality (Brandes).
//! * `hub_nodes`       — top-K nodes by weighted degree.
//!
//! Pure function of its inputs; the scanner holds no mutable state.

#![allow(missing_docs)] // public struct fields are self-documenting (qualified_name, kind, …)

use std::collections::{HashMap, HashSet, VecDeque};

use petgraph::graph::{NodeIndex, UnGraph};
use serde::{Deserialize, Serialize};

/// One node in the architecture graph. Equivalent of a row in
/// `graph.db::nodes` filtered to callable symbols.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchNode {
    pub qualified_name: String,
    pub kind: String,
    pub file: String,
    pub community_id: u32,
    /// How many callers point at this node (in-degree on the `calls` edge
    /// kind). Pre-computed by the caller.
    pub caller_count: u32,
    /// 0..1 criticality score — higher = more critical.
    pub criticality: f32,
    /// Whether any security finding touches this node.
    pub security_flag: bool,
}

/// One edge in the architecture graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchEdge {
    pub source: String,
    pub target: String,
    pub kind: String,
    pub weight: f32,
}

/// Dense coupling matrix row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CouplingRow {
    pub from_community: u32,
    pub to_community: u32,
    pub edge_count: u32,
    /// `edge_count` divided by (|from| * |to|) — the density.
    pub density: f32,
}

/// Risk ranking entry for one community.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommunityRisk {
    pub community_id: u32,
    pub total_callers: u32,
    pub avg_criticality: f32,
    pub security_hits: u32,
    /// The final composite risk index.
    pub risk_index: f32,
    pub top_symbols: Vec<String>,
}

/// Betweenness-centrality bridge node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeNode {
    pub qualified_name: String,
    pub community_id: u32,
    pub betweenness: f32,
}

/// Degree-based hub node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubNode {
    pub qualified_name: String,
    pub community_id: u32,
    pub degree: u32,
}

/// Complete architecture overview, ready to be serialised into the
/// `architecture_snapshots` JSON columns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchitectureOverview {
    pub community_count: u32,
    pub node_count: u32,
    pub edge_count: u32,
    pub coupling_matrix: Vec<CouplingRow>,
    pub risk_index: Vec<CommunityRisk>,
    pub bridge_nodes: Vec<BridgeNode>,
    pub hub_nodes: Vec<HubNode>,
}

/// Stateless analyser.
#[derive(Debug, Clone, Default)]
pub struct ArchitectureScanner;

impl ArchitectureScanner {
    /// New scanner.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Run the full analysis.
    #[must_use]
    pub fn analyze(&self, nodes: &[ArchNode], edges: &[ArchEdge]) -> ArchitectureOverview {
        if nodes.is_empty() {
            return ArchitectureOverview {
                community_count: 0,
                node_count: 0,
                edge_count: 0,
                coupling_matrix: Vec::new(),
                risk_index: Vec::new(),
                bridge_nodes: Vec::new(),
                hub_nodes: Vec::new(),
            };
        }

        // Map qualified_name -> index.
        let mut idx_of: HashMap<String, usize> = HashMap::with_capacity(nodes.len());
        for (i, n) in nodes.iter().enumerate() {
            idx_of.insert(n.qualified_name.clone(), i);
        }

        // Build petgraph for betweenness + degree.
        let mut g: UnGraph<usize, f32> = UnGraph::with_capacity(nodes.len(), edges.len());
        let mut node_ix: Vec<NodeIndex> = Vec::with_capacity(nodes.len());
        for i in 0..nodes.len() {
            node_ix.push(g.add_node(i));
        }
        let mut edge_count: u32 = 0;
        for e in edges {
            let si_opt = idx_of.get(&e.source);
            let ti_opt = idx_of.get(&e.target);
            let (si, ti) = match (si_opt, ti_opt) {
                (Some(&a), Some(&b)) => (a, b),
                _ => continue,
            };
            if si == ti {
                continue;
            }
            g.add_edge(node_ix[si], node_ix[ti], e.weight.max(1e-6));
            edge_count += 1;
        }

        let coupling_matrix = build_coupling_matrix(nodes, edges);
        let risk_index = build_risk_index(nodes);
        let bridge_nodes = betweenness_top_k(&g, nodes, 10);
        let hub_nodes = degree_top_k(nodes, edges, 10);
        let community_count = nodes
            .iter()
            .map(|n| n.community_id)
            .collect::<HashSet<_>>()
            .len() as u32;

        ArchitectureOverview {
            community_count,
            node_count: nodes.len() as u32,
            edge_count,
            coupling_matrix,
            risk_index,
            bridge_nodes,
            hub_nodes,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_coupling_matrix(nodes: &[ArchNode], edges: &[ArchEdge]) -> Vec<CouplingRow> {
    let mut comm_size: HashMap<u32, u32> = HashMap::new();
    let mut comm_of: HashMap<String, u32> = HashMap::new();
    for n in nodes {
        *comm_size.entry(n.community_id).or_default() += 1;
        comm_of.insert(n.qualified_name.clone(), n.community_id);
    }
    // Cross-community edge counts, canonicalised so (a <= b).
    let mut counts: HashMap<(u32, u32), u32> = HashMap::new();
    for e in edges {
        let ca_opt = comm_of.get(&e.source);
        let cb_opt = comm_of.get(&e.target);
        let (ca, cb) = match (ca_opt, cb_opt) {
            (Some(&a), Some(&b)) => (a, b),
            _ => continue,
        };
        let (a, b) = if ca <= cb { (ca, cb) } else { (cb, ca) };
        *counts.entry((a, b)).or_default() += 1;
    }
    let mut out = Vec::with_capacity(counts.len());
    for ((a, b), cnt) in counts {
        let na = *comm_size.get(&a).unwrap_or(&0) as f32;
        let nb = *comm_size.get(&b).unwrap_or(&0) as f32;
        let denom = if a == b {
            (na * (na - 1.0)).max(1.0)
        } else {
            (na * nb).max(1.0)
        };
        out.push(CouplingRow {
            from_community: a,
            to_community: b,
            edge_count: cnt,
            density: (cnt as f32) / denom,
        });
    }
    out.sort_by_key(|x| (x.from_community, x.to_community));
    out
}

#[allow(clippy::type_complexity)]
fn build_risk_index(nodes: &[ArchNode]) -> Vec<CommunityRisk> {
    let mut acc: HashMap<u32, (u32, f32, u32, Vec<(String, u32)>)> = HashMap::new();
    for n in nodes {
        let entry = acc.entry(n.community_id).or_default();
        entry.0 += n.caller_count;
        entry.1 += n.criticality;
        if n.security_flag {
            entry.2 += 1;
        }
        entry.3.push((n.qualified_name.clone(), n.caller_count));
    }
    let mut out = Vec::with_capacity(acc.len());
    for (cid, (callers, crit_sum, sec_hits, mut syms)) in acc {
        let count = syms.len() as f32;
        let avg_crit = if count > 0.0 { crit_sum / count } else { 0.0 };
        let security_multiplier = if sec_hits > 0 {
            1.0 + (sec_hits as f32) * 0.5
        } else {
            1.0
        };
        let risk = (callers as f32) * (0.1 + avg_crit) * security_multiplier;
        syms.sort_by_key(|s| std::cmp::Reverse(s.1));
        let top: Vec<String> = syms.into_iter().take(5).map(|(s, _)| s).collect();
        out.push(CommunityRisk {
            community_id: cid,
            total_callers: callers,
            avg_criticality: avg_crit,
            security_hits: sec_hits,
            risk_index: risk,
            top_symbols: top,
        });
    }
    out.sort_by(|a, b| {
        b.risk_index
            .partial_cmp(&a.risk_index)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// Degree top-K hubs.
fn degree_top_k(nodes: &[ArchNode], edges: &[ArchEdge], k: usize) -> Vec<HubNode> {
    let mut deg: HashMap<String, u32> = HashMap::new();
    for e in edges {
        *deg.entry(e.source.clone()).or_default() += 1;
        *deg.entry(e.target.clone()).or_default() += 1;
    }
    let mut hubs: Vec<HubNode> = nodes
        .iter()
        .map(|n| HubNode {
            qualified_name: n.qualified_name.clone(),
            community_id: n.community_id,
            degree: *deg.get(&n.qualified_name).unwrap_or(&0),
        })
        .collect();
    hubs.sort_by_key(|h| std::cmp::Reverse(h.degree));
    hubs.truncate(k);
    hubs
}

/// Approximate betweenness centrality (Brandes 2001). O(V * (V + E)).
/// Capped at 2,000 source nodes to keep compute bounded.
///
/// A3-016 (2026-05-04): when V > 2000 the original implementation took
/// the FIRST 2,000 nodes by petgraph iteration order, biasing the
/// betweenness scores toward whichever 2,000 nodes happened to come
/// first (effectively insertion order). On a graph of 30,000 nodes that
/// computed centrality against a 6.7% sample with strong ordering bias,
/// silently producing a partial result.
///
/// Fix: deterministic even-spaced stride sample. When V <= 2000 we
/// process every source; when V > 2000 we step through with stride =
/// V / 2000 so the sample covers the full node-index range. Determinism
/// keeps two runs of the same audit producing identical bridge_nodes.
/// Trade-off: still a sample, not full O(V * (V + E)). For exact
/// betweenness on large graphs, raise the cap.
fn betweenness_top_k(g: &UnGraph<usize, f32>, nodes: &[ArchNode], k: usize) -> Vec<BridgeNode> {
    let n = g.node_count();
    if n == 0 {
        return Vec::new();
    }
    let mut cb: Vec<f64> = vec![0.0; n];
    const SAMPLE_CAP: usize = 2_000;
    let sources: Vec<NodeIndex> = if n <= SAMPLE_CAP {
        g.node_indices().collect()
    } else {
        let stride = (n / SAMPLE_CAP).max(1);
        g.node_indices().step_by(stride).take(SAMPLE_CAP).collect()
    };

    for s in sources {
        let mut stack: Vec<NodeIndex> = Vec::with_capacity(n);
        let mut preds: Vec<Vec<NodeIndex>> = vec![Vec::new(); n];
        let mut sigma: Vec<f64> = vec![0.0; n];
        let mut dist: Vec<i64> = vec![-1; n];
        sigma[s.index()] = 1.0;
        dist[s.index()] = 0;
        let mut queue: VecDeque<NodeIndex> = VecDeque::new();
        queue.push_back(s);
        while let Some(v) = queue.pop_front() {
            stack.push(v);
            for w in g.neighbors(v) {
                if dist[w.index()] < 0 {
                    dist[w.index()] = dist[v.index()] + 1;
                    queue.push_back(w);
                }
                if dist[w.index()] == dist[v.index()] + 1 {
                    sigma[w.index()] += sigma[v.index()];
                    preds[w.index()].push(v);
                }
            }
        }
        let mut delta: Vec<f64> = vec![0.0; n];
        while let Some(w) = stack.pop() {
            for &v in &preds[w.index()] {
                if sigma[w.index()] > 0.0 {
                    delta[v.index()] +=
                        (sigma[v.index()] / sigma[w.index()]) * (1.0 + delta[w.index()]);
                }
            }
            if w != s {
                cb[w.index()] += delta[w.index()];
            }
        }
    }

    let max_cb = cb.iter().cloned().fold(0.0_f64, f64::max);
    let norm = if max_cb > 0.0 { max_cb } else { 1.0 };

    let mut out: Vec<BridgeNode> = g
        .node_indices()
        .map(|ni| {
            let payload = *g.node_weight(ni).expect("weight");
            let node = &nodes[payload];
            BridgeNode {
                qualified_name: node.qualified_name.clone(),
                community_id: node.community_id,
                betweenness: (cb[ni.index()] / norm) as f32,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.betweenness
            .partial_cmp(&a.betweenness)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(k);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_node(q: &str, c: u32, callers: u32) -> ArchNode {
        ArchNode {
            qualified_name: q.to_string(),
            kind: "function".to_string(),
            file: "f.ts".to_string(),
            community_id: c,
            caller_count: callers,
            criticality: 0.5,
            security_flag: false,
        }
    }

    fn mk_edge(s: &str, t: &str) -> ArchEdge {
        ArchEdge {
            source: s.to_string(),
            target: t.to_string(),
            kind: "calls".to_string(),
            weight: 1.0,
        }
    }

    #[test]
    fn empty_inputs_produce_empty_overview() {
        let out = ArchitectureScanner::new().analyze(&[], &[]);
        assert_eq!(out.node_count, 0);
        assert!(out.coupling_matrix.is_empty());
    }

    #[test]
    fn simple_chain_has_bridge() {
        let nodes = vec![
            mk_node("a", 0, 0),
            mk_node("b", 0, 1),
            mk_node("c", 1, 1),
            mk_node("d", 1, 0),
        ];
        let edges = vec![mk_edge("a", "b"), mk_edge("b", "c"), mk_edge("c", "d")];
        let out = ArchitectureScanner::new().analyze(&nodes, &edges);
        assert_eq!(out.community_count, 2);
        assert!(!out.bridge_nodes.is_empty());
        assert!(out
            .coupling_matrix
            .iter()
            .any(|r| r.from_community == 0 && r.to_community == 1));
    }
}
