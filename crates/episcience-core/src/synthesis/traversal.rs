use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeType {
    Supports,
    Contradicts,
    Supersedes,
    Methodology,
    Corroborates,
}

impl EdgeType {
    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeType::Supports => "SUPPORTS",
            EdgeType::Contradicts => "CONTRADICTS",
            // The kernel's canonical relationship spelling (lower-case).
            EdgeType::Supersedes => "supersedes",
            EdgeType::Methodology => "METHODOLOGY",
            EdgeType::Corroborates => "CORROBORATES",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraversalConfig {
    pub max_hops: u8,
    pub edge_types: Vec<EdgeType>,
    pub follow_via_paper: bool,
    pub relevance_prune: f64,
    pub max_subgraph_size: usize,
}

impl Default for TraversalConfig {
    fn default() -> Self {
        Self {
            max_hops: 2,
            edge_types: vec![
                EdgeType::Supports,
                EdgeType::Contradicts,
                EdgeType::Supersedes,
                EdgeType::Methodology,
                EdgeType::Corroborates,
            ],
            follow_via_paper: true,
            relevance_prune: 0.3,
            max_subgraph_size: 500,
        }
    }
}

#[allow(clippy::double_must_use)] // async_trait's generated #[must_use] on an already-must-use boxed future
#[async_trait]
pub trait EdgeProvider: Send + Sync {
    async fn neighbors(&self, claim: Uuid, types: &[EdgeType]) -> Vec<(Uuid, EdgeType)>;
}

pub async fn traverse<P, Sim, Fut>(
    seeds: &[Uuid],
    cfg: &TraversalConfig,
    provider: &P,
    relevance: Sim,
) -> Result<crate::synthesis::SubgraphSnapshot, crate::synthesis::errors::SynthesisError>
where
    P: EdgeProvider,
    Sim: Fn(Uuid) -> Fut,
    Fut: std::future::Future<Output = f64>,
{
    use std::collections::{HashSet, VecDeque};

    let mut visited: HashSet<Uuid> = seeds.iter().copied().collect();
    let mut frontier: VecDeque<(Uuid, u8)> = seeds.iter().map(|&id| (id, 0u8)).collect();

    while let Some((claim, hop)) = frontier.pop_front() {
        if visited.len() >= cfg.max_subgraph_size {
            break;
        }
        if hop >= cfg.max_hops {
            continue;
        }
        let neighbors = provider.neighbors(claim, &cfg.edge_types).await;
        for (n, _t) in neighbors {
            if visited.contains(&n) {
                continue;
            }
            if relevance(n).await < cfg.relevance_prune {
                continue;
            }
            visited.insert(n);
            frontier.push_back((n, hop + 1));
            if visited.len() >= cfg.max_subgraph_size {
                break;
            }
        }
    }

    Ok(crate::synthesis::SubgraphSnapshot {
        claim_ids: visited.into_iter().collect(),
        edge_ids: vec![], // populated by caller from edge metadata
        belief_intervals: vec![],
        traversal_config: serde_json::to_value(cfg).unwrap(),
        captured_at: chrono::Utc::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct InMemEdges {
        adj: HashMap<Uuid, Vec<(Uuid, EdgeType)>>,
    }

    #[async_trait]
    impl EdgeProvider for InMemEdges {
        async fn neighbors(&self, claim: Uuid, types: &[EdgeType]) -> Vec<(Uuid, EdgeType)> {
            self.adj
                .get(&claim)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|(_, t)| types.contains(t))
                .collect()
        }
    }

    /// An edge store keyed by the kernel's relationship STRING, the way a
    /// real provider filters `claim_relationships.relationship = ANY($1)`
    /// with the requested types' `as_str()`. A row is followed only when
    /// some requested type spells its relationship exactly.
    struct RelationshipRows {
        rows: Vec<(Uuid, Uuid, &'static str)>,
    }

    #[async_trait]
    impl EdgeProvider for RelationshipRows {
        async fn neighbors(&self, claim: Uuid, types: &[EdgeType]) -> Vec<(Uuid, EdgeType)> {
            self.rows
                .iter()
                .filter(|(src, _, _)| *src == claim)
                .filter_map(|(_, dst, rel)| {
                    types
                        .iter()
                        .find(|t| t.as_str() == *rel)
                        .map(|t| (*dst, *t))
                })
                .collect()
        }
    }

    #[test]
    fn supersedes_uses_the_kernel_lower_case_spelling() {
        assert_eq!(EdgeType::Supersedes.as_str(), "supersedes");
    }

    #[tokio::test]
    async fn supersedes_only_traversal_follows_a_lower_case_supersedes_row() {
        // The kernel writes lineage edges as lower-case `supersedes`. A
        // Supersedes-only traversal (registry_diff's shape) must reach the
        // prior version through such a row; the seed has no other edge.
        let newer = Uuid::from_u128(10);
        let older = Uuid::from_u128(11);
        let provider = RelationshipRows {
            rows: vec![(newer, older, "supersedes")],
        };
        let cfg = TraversalConfig {
            max_hops: 1,
            edge_types: vec![EdgeType::Supersedes],
            ..TraversalConfig::default()
        };
        let snap = traverse(&[newer], &cfg, &provider, |_| async { 1.0 })
            .await
            .unwrap();
        assert!(
            snap.claim_ids.contains(&older),
            "lower-case `supersedes` row not followed; reached {:?}",
            snap.claim_ids
        );
        assert_eq!(snap.claim_ids.len(), 2);
    }

    #[tokio::test]
    async fn bfs_respects_max_hops() {
        let a = Uuid::nil();
        let b = Uuid::from_u128(1);
        let c = Uuid::from_u128(2);
        let provider = InMemEdges {
            adj: vec![
                (a, vec![(b, EdgeType::Supports)]),
                (b, vec![(c, EdgeType::Supports)]),
            ]
            .into_iter()
            .collect(),
        };
        let cfg = TraversalConfig {
            max_hops: 1,
            ..TraversalConfig::default()
        };
        let snap = traverse(&[a], &cfg, &provider, |_| async { 1.0 })
            .await
            .unwrap();
        assert_eq!(snap.claim_ids.len(), 2); // a + b, not c
    }

    #[tokio::test]
    async fn bfs_prunes_neighbors_below_relevance_threshold() {
        let a = Uuid::nil();
        let b = Uuid::from_u128(1);
        let c = Uuid::from_u128(2);
        let provider = InMemEdges {
            adj: vec![(a, vec![(b, EdgeType::Supports), (c, EdgeType::Supports)])]
                .into_iter()
                .collect(),
        };
        let cfg = TraversalConfig {
            relevance_prune: 0.5,
            ..TraversalConfig::default()
        };
        let relevance = |id: Uuid| async move {
            if id == Uuid::from_u128(1) {
                0.9
            } else {
                0.1
            }
        };
        let snap = traverse(&[a], &cfg, &provider, relevance).await.unwrap();
        assert!(snap.claim_ids.contains(&a));
        assert!(snap.claim_ids.contains(&b));
        assert!(!snap.claim_ids.contains(&c));
    }

    #[tokio::test]
    async fn bfs_caps_at_max_subgraph_size() {
        // Construct a fan-out of 600 nodes from a single seed.
        // With max_subgraph_size=500 (default), traversal must stop at 500.
        let seed = Uuid::from_u128(0);
        let mut adj: HashMap<Uuid, Vec<(Uuid, EdgeType)>> = HashMap::new();
        let neighbors: Vec<(Uuid, EdgeType)> = (1u128..=600)
            .map(|i| (Uuid::from_u128(i), EdgeType::Supports))
            .collect();
        adj.insert(seed, neighbors);

        let provider = InMemEdges { adj };
        let cfg = TraversalConfig::default(); // max_subgraph_size = 500
        let snap = traverse(&[seed], &cfg, &provider, |_| async { 1.0 })
            .await
            .unwrap();
        assert!(
            snap.claim_ids.len() <= 500,
            "expected <= 500, got {}",
            snap.claim_ids.len()
        );
    }
}
