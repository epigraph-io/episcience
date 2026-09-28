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

/// Who may read an EpiScience row besides its owner group's members: the
/// kernel's tenancy vocabulary.
///
/// * `Group` — readable by members of the row's owner group only.
/// * `Public` — readable by everyone.
///
/// The legacy vocabulary is still READ (legacy rows keep `private` / `shared`
/// until the contract migration converts them): both mean `Group`. A request
/// may say `private` (an alias of `group`); `shared` is retired at the API
/// (410), because the kernel's only sharing primitive is group ownership.
/// Writes always emit `group` or `public`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    #[serde(alias = "private")]
    Group,
    Public,
}

impl std::str::FromStr for Visibility {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "group" | "private" | "shared" => Ok(Self::Group),
            "public" => Ok(Self::Public),
            _ => Err(format!("unknown Visibility: {s}")),
        }
    }
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Group => "group",
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

/// Pure-Rust mirror of the read predicate every EpiScience read splices in
/// (the kernel's `Viewer::splice`, `/* {VISIBILITY:s} */`):
///
/// ```sql
/// visibility = 'public' OR owner_group_id = ANY($viewer_groups)
/// ```
///
/// Extracted as a pure function so property tests (see
/// `episcience-core::synthesis::proptest`) can exercise it without Postgres.
/// Authorship plays no part: a synthesis its author created in a team group
/// the author later left is no longer the author's to read.
pub fn read_predicate(
    visibility: Visibility,
    owner_group_id: Uuid,
    viewer_groups: &[Uuid],
) -> bool {
    visibility == Visibility::Public || viewer_groups.contains(&owner_group_id)
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
    /// The owning group (kernel `groups.id`). `None` only for a legacy row
    /// between the expand migration and the one-shot re-own.
    pub owner_group_id: Option<Uuid>,
    pub failure_reason: Option<String>,
    /// Autonomy level that produced this synthesis: "co_pilot" | "autopilot" | "autonomous".
    /// `None` is equivalent to "autopilot" (group visibility, countersign required).
    pub autonomy_level: Option<String>,
}

/// A recorded staleness event for a synthesis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StalenessEvent {
    pub id: Uuid,
    pub synthesis_id: Uuid,
    pub detected_at: DateTime<Utc>,
    /// One of: 'belief_drift', 'new_contradiction', 'claim_superseded',
    ///         'frame_changed', 'edge_revoked', 'input_narrowed'
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

    /// The legacy vocabulary still reads (rows in the deploy window), a
    /// request's `private` is an alias of `group`, and writes emit only the
    /// kernel's two words. Kills: `private` parsed as an error (legacy rows
    /// would fail to load), `shared` accepted on the wire (it is retired),
    /// or `as_str` emitting a legacy word (the contract CHECK refuses it).
    #[test]
    fn visibility_reads_the_legacy_words_and_writes_the_kernel_ones() {
        for (db, want) in [
            ("group", Visibility::Group),
            ("private", Visibility::Group),
            ("shared", Visibility::Group),
            ("public", Visibility::Public),
        ] {
            assert_eq!(db.parse::<Visibility>().unwrap(), want, "{db}");
        }
        assert!("world".parse::<Visibility>().is_err());
        assert_eq!(
            serde_json::from_str::<Visibility>("\"private\"").unwrap(),
            Visibility::Group
        );
        assert_eq!(
            serde_json::from_str::<Visibility>("\"group\"").unwrap(),
            Visibility::Group
        );
        assert!(serde_json::from_str::<Visibility>("\"shared\"").is_err());
        assert_eq!(Visibility::Group.as_str(), "group");
        assert_eq!(
            serde_json::to_string(&Visibility::Group).unwrap(),
            "\"group\""
        );
        assert_eq!(Visibility::Public.as_str(), "public");
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
