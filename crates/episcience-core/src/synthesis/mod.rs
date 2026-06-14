//! Paper-synthesis core types — pure data + state, no I/O.

pub mod clustering;
pub mod errors;
#[cfg(feature = "test-utils")]
pub mod mock_llm;
pub mod novelty;
#[cfg(test)]
mod proptest;
pub mod refinement;
pub mod skill;
pub mod skills;
pub mod traversal;
pub mod util;
pub mod verifier;
// TODO(Phase 2/4): pub mod staleness;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SynthesisStatus {
    Pending,
    Running,
    /// Stage 6 verifier is evaluating the composed narrative (transient).
    /// Phase 4 added this variant to the DB CHECK constraint (migration
    /// 5021); the worker may set this status during long-running verifies.
    Verifying,
    Complete,
    Failed,
    Deleted,
    /// Stage 6 verifier rejected the narrative. Terminal until Phase 7
    /// ships refinement, which will create a child synthesis via
    /// `synthesis_provo_edges` predicate=`REFINES` while leaving this row
    /// in `rejected`.
    Rejected,
}

impl std::str::FromStr for SynthesisStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "verifying" => Ok(Self::Verifying),
            "complete" => Ok(Self::Complete),
            "failed" => Ok(Self::Failed),
            "deleted" => Ok(Self::Deleted),
            "rejected" => Ok(Self::Rejected),
            _ => Err(format!("unknown SynthesisStatus: {s}")),
        }
    }
}

impl SynthesisStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Verifying => "verifying",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Deleted => "deleted",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Private,
    Shared,
    Public,
}

impl std::str::FromStr for Visibility {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "private" => Ok(Self::Private),
            "shared" => Ok(Self::Shared),
            "public" => Ok(Self::Public),
            _ => Err(format!("unknown Visibility: {s}")),
        }
    }
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Shared => "shared",
            Self::Public => "public",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BeliefIntervalEntry {
    pub claim_id: Uuid,
    pub frame_id: Option<Uuid>,
    pub belief: f64,
    pub plausibility: f64,
    pub pignistic_prob: f64,
    pub framed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubgraphSnapshot {
    pub claim_ids: Vec<Uuid>,
    pub edge_ids: Vec<Uuid>,
    pub belief_intervals: Vec<BeliefIntervalEntry>,
    pub traversal_config: serde_json::Value,
    pub captured_at: DateTime<Utc>,
}

/// Pure-Rust mirror of the SQL read predicate enforced by
/// [`crate::SynthesisRepository::readable_by`] (in `episcience-db`):
///
/// ```sql
/// visibility = 'public'
///   OR agent_id = $agent
///   OR (sh.synthesis_id IS NOT NULL AND sh.permission = 'read')
/// ```
///
/// Extracted as a pure function so it can be exercised directly by
/// property tests (see `episcience-core::synthesis::proptest`) without
/// hitting Postgres. Note: `Visibility::Private` and `Visibility::Shared`
/// are indistinguishable in this predicate — only the owner / public flag
/// / share-row gates access. The `Shared` enum variant is documentary
/// (it signals the owner's intent to share rather than enforcing it).
pub fn read_predicate(
    visibility: Visibility,
    owner_id: Uuid,
    agent_id: Uuid,
    has_share: bool,
) -> bool {
    visibility == Visibility::Public || agent_id == owner_id || has_share
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
    /// 'claim' | 'synthesis' | 'agent' | 'workflow'
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
    pub failure_reason: Option<String>,
}

/// A recorded staleness event for a synthesis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StalenessEvent {
    pub id: Uuid,
    pub synthesis_id: Uuid,
    pub detected_at: DateTime<Utc>,
    /// One of: 'belief_drift', 'new_contradiction', 'claim_superseded',
    ///         'frame_changed', 'edge_revoked'
    pub trigger: String,
    pub affected_claim_ids: Vec<Uuid>,
    pub detail: Option<serde_json::Value>,
}

/// Worker position in an event stream (used by WorkerStateRepository).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerState {
    pub worker_id: String,
    pub last_event_id: Option<String>,
    pub last_event_ts: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
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
