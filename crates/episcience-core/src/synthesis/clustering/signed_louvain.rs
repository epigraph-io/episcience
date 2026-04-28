//! Vendored signed-Louvain helpers (~200 LOC).
//!
//! Decision (per plan §1.12): post-hoc separation, not Traag-signed-Louvain.
//! Rationale: reuses stable positive-only Louvain, isolates signed-graph
//! subtlety into one testable separation step. No maintained Rust
//! signed-Louvain crate exists; this ~200-LOC vendor has smaller surface.
//!
//! All tie-breaks use Uuid ordering for cross-run determinism (HashMap
//! iteration is non-deterministic; all iteration is done on sorted slices).

use std::collections::HashMap;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build adjacency list (both directed sides stored) for positive weights.
fn positive_adj(
    claims: &[Uuid],
    signed_edges: &[(Uuid, Uuid, f64)],
) -> HashMap<Uuid, Vec<(Uuid, f64)>> {
    let claim_set: std::collections::HashSet<Uuid> = claims.iter().copied().collect();
    let mut adj: HashMap<Uuid, Vec<(Uuid, f64)>> =
        claims.iter().map(|&id| (id, Vec::new())).collect();
    for &(u, v, w) in signed_edges {
        if w <= 0.0 {
            continue;
        }
        if claim_set.contains(&u) && claim_set.contains(&v) {
            adj.entry(u).or_default().push((v, w));
            adj.entry(v).or_default().push((u, w));
        }
    }
    adj
}

/// Total weight of all edges incident to `node`.
fn node_strength(node: Uuid, adj: &HashMap<Uuid, Vec<(Uuid, f64)>>) -> f64 {
    adj.get(&node)
        .map(|ns| ns.iter().map(|(_, w)| w).sum())
        .unwrap_or(0.0)
}

/// Total weight of all edges in the graph (each edge counted once from both sides → sum/2).
fn total_weight(adj: &HashMap<Uuid, Vec<(Uuid, f64)>>) -> f64 {
    adj.values()
        .flat_map(|ns| ns.iter().map(|(_, w)| w))
        .sum::<f64>()
        / 2.0
}

/// Sum of positive-edge weights between `node` and members of `community`.
fn weight_to_community(
    node: Uuid,
    community: &[Uuid],
    adj: &HashMap<Uuid, Vec<(Uuid, f64)>>,
) -> f64 {
    let comm_set: std::collections::HashSet<Uuid> = community.iter().copied().collect();
    adj.get(&node)
        .map(|ns| {
            ns.iter()
                .filter(|(n, _)| comm_set.contains(n))
                .map(|(_, w)| w)
                .sum()
        })
        .unwrap_or(0.0)
}

/// Sum of strengths of all nodes in `community` (Sigma_tot in modularity formula).
fn community_strength(community: &[Uuid], adj: &HashMap<Uuid, Vec<(Uuid, f64)>>) -> f64 {
    community.iter().map(|&n| node_strength(n, adj)).sum()
}

// ---------------------------------------------------------------------------
// 1. Positive-only Louvain
// ---------------------------------------------------------------------------

/// Iterative greedy modularity optimisation on positive edges only.
/// Nodes are processed in sorted-Uuid order; ties are broken by Uuid ordering.
/// Returns a partition of `claims` as a `Vec<Vec<Uuid>>` (each inner Vec sorted).
pub fn louvain_positive(claims: &[Uuid], signed_edges: &[(Uuid, Uuid, f64)]) -> Vec<Vec<Uuid>> {
    if claims.is_empty() {
        return vec![];
    }

    let adj = positive_adj(claims, signed_edges);
    let m = total_weight(&adj);

    // Assign each node to its own singleton community.
    // community_of[node] = community_id (represented as min-Uuid of original members)
    let mut community_of: HashMap<Uuid, Uuid> = claims.iter().map(|&id| (id, id)).collect();

    // Sorted node list for deterministic iteration.
    let mut sorted_nodes: Vec<Uuid> = claims.to_vec();
    sorted_nodes.sort();

    let mut improved = true;
    while improved {
        improved = false;

        for &node in &sorted_nodes {
            let current_comm_id = community_of[&node];

            // Build list of neighboring community ids (distinct, sorted for determinism).
            let neighbor_comm_ids: Vec<Uuid> = {
                let mut ids: Vec<Uuid> = adj
                    .get(&node)
                    .map(|ns| ns.iter().map(|(n, _)| community_of[n]).collect())
                    .unwrap_or_default();
                ids.push(current_comm_id); // include own community
                ids.sort();
                ids.dedup();
                ids
            };

            if neighbor_comm_ids.len() == 1 {
                // Only own community — no move possible.
                continue;
            }

            // For each candidate community, compute delta-Q from moving `node` into it.
            // Formula: ΔQ = k_i_in / m - (sigma_tot * k_i) / (2m²)
            //   k_i_in: weight of edges from node to the candidate community
            //   sigma_tot: total strength of the candidate community (excluding node if same)
            //   k_i: strength of node
            //
            // We compare deltas relative to removing node from current community (ΔQ_remove)
            // and adding to best candidate. Use the standard Louvain ΔQ formulation:
            // net gain = delta_add(best) - delta_add(current) where delta_add =
            //   [k_i_in - sigma_tot * k_i / (2m)] / m

            if m == 0.0 {
                // No positive edges at all — no moves improve modularity.
                break;
            }

            let k_i = node_strength(node, &adj);

            // Collect members per candidate community.
            let mut comm_members: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
            for &n in &sorted_nodes {
                comm_members.entry(community_of[&n]).or_default().push(n);
            }

            // Compute delta for removing node from current community and adding to each candidate.
            let best_comm = {
                let mut best_delta = 0.0_f64; // must beat 0 to justify move
                let mut best_id = current_comm_id;

                // Delta for each candidate (including current — nets to 0 → won't replace).
                for &cid in &neighbor_comm_ids {
                    if cid == current_comm_id {
                        continue;
                    }
                    let comm = comm_members.get(&cid).map(|v| v.as_slice()).unwrap_or(&[]);
                    let k_in = weight_to_community(node, comm, &adj);
                    let sigma_tot = community_strength(comm, &adj);

                    // Also compute cost of leaving current community.
                    let curr_comm = comm_members
                        .get(&current_comm_id)
                        .map(|v| v.as_slice())
                        .unwrap_or(&[]);
                    let k_in_curr = weight_to_community(node, curr_comm, &adj)
                        - adj
                            .get(&node)
                            .map(|ns| {
                                ns.iter()
                                    .filter(|(n, _)| *n == node)
                                    .map(|(_, w)| w)
                                    .sum::<f64>()
                            })
                            .unwrap_or(0.0);
                    let sigma_tot_curr = community_strength(curr_comm, &adj) - k_i;

                    let delta_add = (k_in - sigma_tot * k_i / (2.0 * m)) / m;
                    let delta_remove = (k_in_curr - sigma_tot_curr * k_i / (2.0 * m)) / m;
                    let net = delta_add - delta_remove;

                    // Tie-break: prefer community with smaller min-Uuid.
                    if net > best_delta || (net == best_delta && cid < best_id) {
                        best_delta = net;
                        best_id = cid;
                    }
                }
                best_id
            };

            if best_comm != current_comm_id {
                community_of.insert(node, best_comm);
                improved = true;
            }
        }
    }

    // Collect communities, sort each, sort the outer vec by min-Uuid for stability.
    let mut comm_map: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for (&node, &cid) in &community_of {
        comm_map.entry(cid).or_default().push(node);
    }
    let mut result: Vec<Vec<Uuid>> = comm_map.into_values().collect();
    for c in &mut result {
        c.sort();
    }
    result.sort_by_key(|c| c[0]);
    result
}

// ---------------------------------------------------------------------------
// 2. Post-hoc CONTRADICTS separation
// ---------------------------------------------------------------------------

/// For each cluster with intra-cluster CONTRADICTS density > `threshold`,
/// split into two groups by greedy bipartition minimising CONTRADICTS-edge cut.
/// Processes nodes in sorted-Uuid order for determinism.
pub fn separate_on_contradicts(
    clusters: Vec<Vec<Uuid>>,
    signed_edges: &[(Uuid, Uuid, f64)],
    threshold: f64,
) -> Vec<Vec<Uuid>> {
    let mut result = Vec::new();

    for cluster in clusters {
        let n = cluster.len();
        if n <= 1 {
            result.push(cluster);
            continue;
        }

        // Count intra-cluster CONTRADICTS edges (negative weight).
        let neg_count = signed_edges
            .iter()
            .filter(|&&(u, v, w)| w < 0.0 && cluster.contains(&u) && cluster.contains(&v))
            .count();

        let density = neg_count as f64 / n as f64;

        if density <= threshold {
            result.push(cluster);
            continue;
        }

        // Greedy bipartition: assign each node (in sorted-Uuid order) to the side
        // that minimises CONTRADICTS edges within the cluster.
        let mut sorted_cluster = cluster.clone();
        sorted_cluster.sort();

        // side: 0 or 1
        let mut side: HashMap<Uuid, u8> = HashMap::new();
        // First node goes to side 0.
        side.insert(sorted_cluster[0], 0);

        for &node in &sorted_cluster[1..] {
            // Count CONTRADICTS edges to each side.
            let mut neg_to: [i32; 2] = [0, 0];
            for &(u, v, w) in signed_edges {
                if w >= 0.0 {
                    continue;
                }
                let (other, this_node) = if u == node {
                    (v, true)
                } else if v == node {
                    (u, true)
                } else {
                    (Uuid::nil(), false)
                };
                if !this_node {
                    continue;
                }
                if let Some(&s) = side.get(&other) {
                    neg_to[s as usize] += 1;
                }
            }
            // Assign to side with fewer CONTRADICTS edges (minimise intra-side conflict).
            // Tie-break: prefer side 0.
            let assign = if neg_to[0] <= neg_to[1] { 0u8 } else { 1u8 };
            side.insert(node, assign);
        }

        let mut group0: Vec<Uuid> = sorted_cluster
            .iter()
            .filter(|&&n| side.get(&n) == Some(&0))
            .copied()
            .collect();
        let mut group1: Vec<Uuid> = sorted_cluster
            .iter()
            .filter(|&&n| side.get(&n) == Some(&1))
            .copied()
            .collect();
        group0.sort();
        group1.sort();

        if !group0.is_empty() {
            result.push(group0);
        }
        if !group1.is_empty() {
            result.push(group1);
        }
    }

    result.sort_by_key(|c| c[0]);
    result
}

// ---------------------------------------------------------------------------
// 3. Merge until cap
// ---------------------------------------------------------------------------

/// Iteratively merge the two smallest clusters until `clusters.len() <= max`.
/// Tie-break by min-Uuid of each cluster.
pub fn merge_until_cap(
    mut clusters: Vec<Vec<Uuid>>,
    _signed_edges: &[(Uuid, Uuid, f64)],
    max: usize,
) -> Vec<Vec<Uuid>> {
    while clusters.len() > max {
        // Find the two smallest clusters. Tie-break by min-Uuid ascending.
        // Sort by (size, min_uuid).
        clusters.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a[0].cmp(&b[0])));

        // Merge the two smallest (index 0 and 1).
        let second = clusters.remove(1);
        clusters[0].extend(second);
        clusters[0].sort();
    }

    clusters.sort_by_key(|c| c[0]);
    clusters
}
