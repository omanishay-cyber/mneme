//! Leiden community detection — pure-Rust, deterministic.
//!
//! Implements the three-phase Leiden algorithm of Traag, Waltman & van Eck
//! (2019, *From Louvain to Leiden: guaranteeing well-connected communities*):
//!
//!   1. **Local moving** — repeatedly move each node to the neighbour
//!      community that yields the largest modularity gain.
//!   2. **Refinement**   — within each community, re-partition into
//!      well-connected sub-communities using a similar local-move pass that
//!      may *only* merge nodes whose move strictly increases modularity.
//!   3. **Aggregation**  — collapse each refined sub-community into a single
//!      super-node and recurse, using the original (not refined) partition
//!      as the starting state.
//!
//! Determinism: every iteration order, tie-break, and random choice runs
//! through a single seeded `ChaCha8` PRNG (default seed = 42).
//!
//! No external community-detection crate is used; the entire implementation
//! is ~300 lines below.

use std::collections::HashMap;

use petgraph::graph::{NodeIndex, UnGraph};
use rand::seq::SliceRandom;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

use crate::error::BrainResult;
use crate::NodeId;

/// Tunable parameters.
#[derive(Debug, Clone, Copy)]
pub struct LeidenConfig {
    /// Resolution parameter γ. Higher ⇒ more, smaller communities.
    pub resolution: f64,
    /// Maximum outer (move/refine/aggregate) iterations before giving up.
    pub max_iterations: usize,
    /// Convergence threshold on modularity gain per outer iteration.
    pub min_delta: f64,
    /// PRNG seed for deterministic ordering.
    pub seed: u64,
}

impl Default for LeidenConfig {
    fn default() -> Self {
        Self {
            resolution: 1.0,
            max_iterations: 16,
            min_delta: 1e-7,
            seed: 42,
        }
    }
}

/// One detected community.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Community {
    /// Stable id (0..num_communities). Not globally unique across runs.
    pub id: u32,
    /// Nodes belonging to this community.
    pub members: Vec<NodeId>,
    /// Cohesion ∈ `[0, 1]`: fraction of weighted incident edges that stay
    /// inside the community. Higher = tighter cluster.
    pub cohesion: f32,
}

/// Solver entry point.
#[derive(Debug, Clone)]
pub struct LeidenSolver {
    cfg: LeidenConfig,
}

impl Default for LeidenSolver {
    fn default() -> Self {
        Self::new(LeidenConfig::default())
    }
}

impl LeidenSolver {
    pub fn new(cfg: LeidenConfig) -> Self {
        Self { cfg }
    }

    /// Run Leiden on a `petgraph::UnGraph<NodeId, f32>`.
    pub fn run(&self, graph: &UnGraph<NodeId, f32>) -> BrainResult<Vec<Community>> {
        let n = graph.node_count();
        if n == 0 {
            return Ok(Vec::new());
        }

        // Project to internal CSR-ish form keyed by 0..n.
        let mut node_ids: Vec<NodeId> = Vec::with_capacity(n);
        let mut idx_of: HashMap<NodeIndex, usize> = HashMap::with_capacity(n);
        for (i, ni) in graph.node_indices().enumerate() {
            node_ids.push(graph[ni]);
            idx_of.insert(ni, i);
        }

        // Adjacency: adj[u] = Vec<(v, w)>
        let mut adj: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
        let mut total_weight: f64 = 0.0;
        for e in graph.edge_indices() {
            // SAFETY: `edge_indices()` yields valid `EdgeIndex` values for
            // edges in the graph; `edge_endpoints` and `edge_weight` are
            // therefore guaranteed to return `Some(_)`. Programmer-impossible
            // None unless `petgraph` violates its invariants.
            let (a, b) = graph
                .edge_endpoints(e)
                .expect("edge index from edge_indices() must resolve");
            let w = *graph
                .edge_weight(e)
                .expect("edge index from edge_indices() must have weight")
                as f64;
            if w <= 0.0 || !w.is_finite() {
                continue;
            }
            let ai = idx_of[&a];
            let bi = idx_of[&b];
            adj[ai].push((bi, w));
            if ai != bi {
                adj[bi].push((ai, w));
            }
            total_weight += w;
        }
        // 2m for undirected (self-loops counted once).
        let two_m = if total_weight > 0.0 {
            2.0 * total_weight
        } else {
            1.0
        };

        let mut rng = ChaCha8Rng::seed_from_u64(self.cfg.seed);
        let partition = leiden_inner(&adj, two_m, &self.cfg, &mut rng);

        // Build Community results from the final flat partition.
        let mut buckets: HashMap<usize, Vec<usize>> = HashMap::new();
        for (node, comm) in partition.iter().enumerate() {
            buckets.entry(*comm).or_default().push(node);
        }

        // Cohesion = inside_weight / total_incident_weight per community.
        let mut sorted_keys: Vec<usize> = buckets.keys().copied().collect();
        sorted_keys.sort_unstable();
        let mut out = Vec::with_capacity(sorted_keys.len());
        for (new_id, k) in sorted_keys.iter().enumerate() {
            let members = &buckets[k];
            let mut inside = 0.0;
            let mut incident = 0.0;
            for &u in members {
                for &(v, w) in &adj[u] {
                    incident += w;
                    if partition[v] == *k {
                        inside += w;
                    }
                }
            }
            let cohesion = if incident > 0.0 {
                (inside / incident) as f32
            } else {
                1.0
            };
            out.push(Community {
                id: new_id as u32,
                members: members.iter().map(|&i| node_ids[i]).collect(),
                cohesion,
            });
        }

        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Core algorithm (operates purely on integer indices)
// ---------------------------------------------------------------------------

fn leiden_inner(
    adj: &[Vec<(usize, f64)>],
    two_m: f64,
    cfg: &LeidenConfig,
    rng: &mut ChaCha8Rng,
) -> Vec<usize> {
    let n = adj.len();
    let mut partition: Vec<usize> = (0..n).collect();

    // Strength (sum of incident weights) per node.
    let strength: Vec<f64> = adj
        .iter()
        .map(|row| row.iter().map(|(_, w)| *w).sum())
        .collect();

    let mut prev_q = modularity(&partition, adj, &strength, two_m, cfg.resolution);

    for _outer in 0..cfg.max_iterations {
        // Phase 1 — local moving.
        local_move(&mut partition, adj, &strength, two_m, cfg, rng);

        // Phase 2 — refinement (subdivide within each community).
        let refined = refine(&partition, adj, &strength, two_m, cfg, rng);

        // Phase 3 — aggregate using `refined` as the new node set, but
        // initialise the next partition from the *original* `partition`.
        // BUG-A2-013 fix: pass `partition` so `aggregate.parent` records the
        // OUTER community label (not a node index). Without this, refined
        // sub-communities from the same outer community got distinct agg
        // labels after `compress`, undoing the Leiden contract.
        let agg = aggregate(adj, &refined, &partition);
        let mut agg_partition: Vec<usize> = (0..agg.adj.len()).collect();
        // Map: each refined community → the (original, pre-refine) community
        // its members belonged to, used to seed the next outer iteration.
        for (refined_id, orig_id) in agg.parent.iter().enumerate() {
            agg_partition[refined_id] = *orig_id;
        }
        // Re-compress so labels are dense.
        compress(&mut agg_partition);

        // Lift agg_partition back to the original node set.
        let mut new_partition = vec![0usize; n];
        for (node, refined_id) in refined.iter().enumerate() {
            new_partition[node] = agg_partition[*refined_id];
        }
        compress(&mut new_partition);
        partition = new_partition;

        let q = modularity(&partition, adj, &strength, two_m, cfg.resolution);
        if (q - prev_q).abs() < cfg.min_delta {
            break;
        }
        prev_q = q;
    }

    compress(&mut partition);
    partition
}

/// Phase 1 — visit every node in randomised order, move it to the
/// neighbour-community giving the largest positive modularity gain.
fn local_move(
    partition: &mut [usize],
    adj: &[Vec<(usize, f64)>],
    strength: &[f64],
    two_m: f64,
    cfg: &LeidenConfig,
    rng: &mut ChaCha8Rng,
) {
    let n = adj.len();
    // Σ strength per community (Σ_tot in the Louvain paper).
    let mut sigma_tot: HashMap<usize, f64> = HashMap::new();
    for (u, c) in partition.iter().enumerate() {
        *sigma_tot.entry(*c).or_default() += strength[u];
    }

    let mut order: Vec<usize> = (0..n).collect();
    order.shuffle(rng);

    let mut moved = true;
    let mut passes = 0usize;
    while moved && passes < 32 {
        moved = false;
        passes += 1;
        for &u in &order {
            let cu = partition[u];
            // Sum of edge weights from u to each neighbour community.
            let mut k_in: HashMap<usize, f64> = HashMap::new();
            for &(v, w) in &adj[u] {
                if v == u {
                    continue;
                }
                *k_in.entry(partition[v]).or_default() += w;
            }
            // Tentatively pull u out of cu.
            let su = strength[u];
            let kic_self = *k_in.get(&cu).unwrap_or(&0.0);
            *sigma_tot.entry(cu).or_default() -= su;

            let mut best = cu;
            let mut best_gain = 0.0;
            // Deterministic tie-break: by community id ascending.
            let mut keys: Vec<usize> = k_in.keys().copied().collect();
            keys.sort_unstable();
            for c in keys {
                let kic = k_in[&c];
                let st = *sigma_tot.get(&c).unwrap_or(&0.0);
                // Modularity delta for moving u into c (Louvain formula).
                let gain = kic / two_m - cfg.resolution * st * su / (two_m * two_m);
                if gain > best_gain + 1e-12 {
                    best_gain = gain;
                    best = c;
                }
            }
            // Cost of leaving cu (for a fair compare include it).
            let stay_gain = kic_self / two_m
                - cfg.resolution * (*sigma_tot.get(&cu).unwrap_or(&0.0)) * su / (two_m * two_m);
            if best != cu && best_gain > stay_gain + 1e-12 {
                partition[u] = best;
                *sigma_tot.entry(best).or_default() += su;
                moved = true;
            } else {
                *sigma_tot.entry(cu).or_default() += su;
            }
        }
    }
}

/// Phase 2 — within each community, restart every node as its own singleton,
/// then perform one move-pass that only allows joining a sub-community when
/// the move strictly increases modularity AND the destination sub-community
/// is itself a connected subgraph of the original community.
fn refine(
    partition: &[usize],
    adj: &[Vec<(usize, f64)>],
    strength: &[f64],
    two_m: f64,
    cfg: &LeidenConfig,
    rng: &mut ChaCha8Rng,
) -> Vec<usize> {
    let n = adj.len();
    let mut refined: Vec<usize> = (0..n).collect(); // start as singletons
                                                    // Compute σ_tot per refined community.
    let mut sigma_tot: HashMap<usize, f64> = (0..n).map(|u| (u, strength[u])).collect();

    let mut order: Vec<usize> = (0..n).collect();
    order.shuffle(rng);

    for &u in &order {
        let cu_outer = partition[u];
        // Candidate destinations: refined communities of neighbours that
        // share the same outer community as u.
        let mut k_in: HashMap<usize, f64> = HashMap::new();
        for &(v, w) in &adj[u] {
            if v == u || partition[v] != cu_outer {
                continue;
            }
            *k_in.entry(refined[v]).or_default() += w;
        }
        let su = strength[u];
        let cu = refined[u];
        *sigma_tot.entry(cu).or_default() -= su;

        let mut best = cu;
        let mut best_gain = 0.0;
        let mut keys: Vec<usize> = k_in.keys().copied().collect();
        keys.sort_unstable();
        for c in keys {
            let kic = k_in[&c];
            let st = *sigma_tot.get(&c).unwrap_or(&0.0);
            let gain = kic / two_m - cfg.resolution * st * su / (two_m * two_m);
            if gain > best_gain + 1e-12 {
                best_gain = gain;
                best = c;
            }
        }
        if best != cu && best_gain > 1e-12 {
            refined[u] = best;
            *sigma_tot.entry(best).or_default() += su;
        } else {
            *sigma_tot.entry(cu).or_default() += su;
        }
    }

    compress(&mut refined);
    refined
}

/// Aggregated graph: each refined community becomes a single super-node.
struct Aggregated {
    /// adjacency among super-nodes
    adj: Vec<Vec<(usize, f64)>>,
    /// `parent[refined_id] = outer_community_id` — the community label the
    /// members of `refined_id` had *before* refinement.
    parent: Vec<usize>,
}

fn aggregate(
    adj: &[Vec<(usize, f64)>],
    refined: &[usize],
    partition: &[usize],
) -> Aggregated {
    let k = refined.iter().copied().max().map(|x| x + 1).unwrap_or(0);
    let mut new_adj: Vec<HashMap<usize, f64>> = vec![HashMap::new(); k];
    for (u, row) in adj.iter().enumerate() {
        let cu = refined[u];
        for &(v, w) in row {
            let cv = refined[v];
            *new_adj[cu].entry(cv).or_default() += w;
        }
    }
    // Halve double-counted off-diagonal weights so total mass is preserved.
    let mut out: Vec<Vec<(usize, f64)>> = Vec::with_capacity(k);
    for (i, m) in new_adj.into_iter().enumerate() {
        let mut row: Vec<(usize, f64)> = m
            .into_iter()
            .map(|(j, w)| (j, if j == i { w } else { w / 2.0 }))
            .collect();
        row.sort_by_key(|&(j, _)| j);
        out.push(row);
    }
    // BUG-A2-013 fix: store the OUTER community label of any member of each
    // refined community. The first member's outer-id is canonical because
    // refinement only subdivides outer communities — every member of
    // `refined_id` belongs to the same outer community.
    let mut parent = vec![usize::MAX; k];
    for (u, c) in refined.iter().enumerate() {
        if parent[*c] == usize::MAX {
            parent[*c] = partition[u];
        }
    }
    Aggregated { adj: out, parent }
}

fn modularity(
    partition: &[usize],
    adj: &[Vec<(usize, f64)>],
    strength: &[f64],
    two_m: f64,
    gamma: f64,
) -> f64 {
    // BUG-A2-014 fix: the inner loop walks every directed edge so each
    // undirected edge is counted twice. The textbook modularity divides
    // the accumulated sum by 2m (a single instance of m, where 2m is the
    // total double-counted weight). Dividing by `two_m` here would scale
    // the doubled sum by 2m which is correct ONLY if the inner sum used
    // each undirected pair once. Since we sum directed edges, the correct
    // normaliser is `2 * two_m` — i.e. divide the accumulated double-
    // counted sum by 4m to match the spec. Concretely: spec is
    // (1/2m) * sum_{i!=j, single-counted}(...); our sum is double-counted
    // so we divide by 2 * (2m).
    let mut q = 0.0;
    for u in 0..adj.len() {
        for &(v, w) in &adj[u] {
            if partition[u] == partition[v] {
                q += w - gamma * strength[u] * strength[v] / two_m;
            }
        }
    }
    q / (2.0 * two_m)
}

/// Renumber labels to be dense `0..k` while preserving partition equivalence.
fn compress(part: &mut [usize]) {
    let mut map: HashMap<usize, usize> = HashMap::new();
    let mut next = 0usize;
    for c in part.iter_mut() {
        let id = *map.entry(*c).or_insert_with(|| {
            let v = next;
            next += 1;
            v
        });
        *c = id;
    }
}
