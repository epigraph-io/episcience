//! Paper-synthesis core types — pure data + state, no I/O.

pub mod errors;
pub mod traversal;
#[cfg(any(test, feature = "test-utils"))]
pub mod mock_llm;
// TODO(Phase 2/4): pub mod pipeline; pub mod staleness;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SynthesisStatus {
    Pending,
    Running,
    Complete,
    Failed,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Private,
    Shared,
    Public,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeliefIntervalEntry {
    pub claim_id: Uuid,
    pub frame_id: Option<Uuid>,
    pub belief: f64,
    pub plausibility: f64,
    pub pignistic_prob: f64,
    pub framed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubgraphSnapshot {
    pub claim_ids: Vec<Uuid>,
    pub edge_ids: Vec<Uuid>,
    pub belief_intervals: Vec<BeliefIntervalEntry>,
    pub traversal_config: serde_json::Value,
    pub captured_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cluster {
    pub id: Uuid,
    pub synthesis_id: Uuid,
    pub cluster_index: i32,
    pub title: String,
    pub summary: String,
    pub member_claim_ids: Vec<Uuid>,
    pub support_count: i32,
    pub contradict_count: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenanceEdge {
    /// 'WAS_DERIVED_FROM' | 'REFINES' | 'COMPOSED_OF' | 'ATTRIBUTED_TO'
    pub predicate: String,
    /// 'claim' | 'synthesis' | 'agent'
    pub target_kind: String,
    pub target_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Synthesis {
    pub id: Uuid,
    pub query: String,
    pub agent_id: Uuid,
    pub status: SynthesisStatus,
    pub parent_synthesis_id: Option<Uuid>,
    pub narrative: Option<String>,
    pub narrative_format: Option<String>,
    pub subgraph_snapshot: SubgraphSnapshot,
    pub clustering_method: String,
    pub llm_provider: String,
    pub llm_model: String,
    pub llm_call_count: i32,
    pub prereq_synthesis_ids: Option<Vec<Uuid>>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub stale_since: Option<DateTime<Utc>>,
    pub stale_reason: Option<String>,
    pub content_hash: Vec<u8>,
    pub visibility: Visibility,
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn synthesis_status_serializes_as_lowercase() {
        let s = serde_json::to_string(&SynthesisStatus::Pending).unwrap();
        assert_eq!(s, "\"pending\"");
        let parsed: SynthesisStatus = serde_json::from_str("\"running\"").unwrap();
        assert!(matches!(parsed, SynthesisStatus::Running));
    }

    #[test]
    fn subgraph_snapshot_round_trips() {
        let snap = SubgraphSnapshot {
            claim_ids: vec![Uuid::nil()],
            edge_ids: vec![Uuid::nil()],
            belief_intervals: vec![],
            traversal_config: serde_json::json!({"max_hops": 2}),
            captured_at: chrono::Utc::now(),
        };
        let s = serde_json::to_string(&snap).unwrap();
        let back: SubgraphSnapshot = serde_json::from_str(&s).unwrap();
        assert_eq!(back.claim_ids, snap.claim_ids);
    }
}
