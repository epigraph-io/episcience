mod signed_louvain;

use uuid::Uuid;

/// Cluster `claims` using positive-only Louvain followed by post-hoc
/// CONTRADICTS-edge separation and finally capping to `max_clusters`.
///
/// `signed_edges`: `(from, to, weight)` triples where
///   +1.0 = SUPPORTS/CORROBORATES, +0.5 = METHODOLOGY, -0.5 = CONTRADICTS.
///
/// Returns a sorted-by-min-uuid `Vec<Vec<Uuid>>` where each inner Vec is a
/// sorted list of claim ids belonging to that cluster.
pub fn cluster_signed(
    claims: &[Uuid],
    signed_edges: &[(Uuid, Uuid, f64)],
    max_clusters: usize,
) -> Vec<Vec<Uuid>> {
    // 1. Run positive-only Louvain on max(w, 0) weights.
    let initial = signed_louvain::louvain_positive(claims, signed_edges);
    // 2. Post-hoc separation: split clusters with high intra-cluster CONTRADICTS density.
    let separated = signed_louvain::separate_on_contradicts(initial, signed_edges, 0.2);
    // 3. Merge smallest clusters until count <= max_clusters.
    signed_louvain::merge_until_cap(separated, signed_edges, max_clusters)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn two_well_separated_groups_become_two_clusters() {
        // Group A: claims 1-3 mutually SUPPORTS
        // Group B: claims 4-6 mutually SUPPORTS
        // No edges between groups
        let edges = vec![
            (id(1), id(2), 1.0),
            (id(2), id(3), 1.0),
            (id(1), id(3), 1.0),
            (id(4), id(5), 1.0),
            (id(5), id(6), 1.0),
            (id(4), id(6), 1.0),
        ];
        let claims = vec![id(1), id(2), id(3), id(4), id(5), id(6)];
        let clusters = cluster_signed(&claims, &edges, 12);
        assert_eq!(clusters.len(), 2);
    }

    #[test]
    fn contradicts_separates_otherwise_connected_claims() {
        // Two claims connected ONLY by CONTRADICTS → must end in separate clusters
        let edges = vec![(id(1), id(2), -0.5)];
        let claims = vec![id(1), id(2)];
        let clusters = cluster_signed(&claims, &edges, 12);
        assert_eq!(clusters.len(), 2);
    }

    #[test]
    fn cluster_count_capped_at_max() {
        // 30 isolated claims → without merge, 30 clusters; cap=12 → ≤ 12 clusters
        let claims: Vec<Uuid> = (1..=30).map(id).collect();
        let edges: Vec<(Uuid, Uuid, f64)> = vec![];
        let clusters = cluster_signed(&claims, &edges, 12);
        assert!(clusters.len() <= 12);
    }

    #[test]
    fn separator_splits_cluster_with_high_contradicts_density() {
        // 4 claims fully connected by positive edges → Louvain forms ONE cluster.
        // Two strong CONTRADICTS edges create intra-cluster neg density = 2/4 = 0.5 > threshold(0.2).
        // separate_on_contradicts must bipartition: id(1),id(2) vs id(3),id(4)
        // such that each CONTRADICTS pair (1,3) and (2,4) lands in different groups.
        //
        // This test exercises separate_on_contradicts non-trivially: the pre-separator
        // partition is one cluster of 4 (not singletons), forcing the greedy bipartition
        // code path to actually run.
        let claims = vec![id(1), id(2), id(3), id(4)];
        let edges = vec![
            // Positive edges: full clique → Louvain forms one cluster
            (id(1), id(2), 1.0),
            (id(2), id(3), 1.0),
            (id(3), id(4), 1.0),
            (id(1), id(3), 1.0),
            (id(2), id(4), 1.0),
            (id(1), id(4), 1.0),
            // Negative edges: two CONTRADICTS pairs that must be separated
            (id(1), id(3), -2.0),
            (id(2), id(4), -2.0),
        ];

        // Verify pre-condition: Louvain alone (positive only) forms ONE cluster of 4.
        let pre_sep = signed_louvain::louvain_positive(&claims, &edges);
        assert_eq!(
            pre_sep.len(),
            1,
            "Pre-condition failed: expected Louvain to form 1 cluster, got {}",
            pre_sep.len()
        );

        // Now run the full pipeline (Louvain + separator + no cap needed).
        let clusters = cluster_signed(&claims, &edges, 12);

        // Separator must split the single cluster into 2.
        assert_eq!(
            clusters.len(),
            2,
            "Expected 2 clusters after separation, got {}",
            clusters.len()
        );

        // Load-bearing: each CONTRADICTS pair must land in different clusters.
        let find_cluster = |target: Uuid| -> usize {
            clusters
                .iter()
                .position(|c| c.contains(&target))
                .expect("claim missing from clusters")
        };
        assert_ne!(
            find_cluster(id(1)),
            find_cluster(id(3)),
            "id(1) and id(3) are CONTRADICTS partners but landed in the same cluster"
        );
        assert_ne!(
            find_cluster(id(2)),
            find_cluster(id(4)),
            "id(2) and id(4) are CONTRADICTS partners but landed in the same cluster"
        );
    }
}
