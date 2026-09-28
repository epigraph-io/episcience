//! Stage 2 (`stage2_traverse`) integration tests for `SynthesisPipeline`.
//!
//! # DB strategy
//!
//! Each test runs on its own clone of the E1 template (`TestDb::fresh`): the
//! kernel schema at the pinned rev plus EpiScience's, seeded with two PUBLIC
//! claims (`aaaa…`, `bbbb…`) and their author, the `f3951e28-…` service agent
//! (scripts/ci-seed.sql). The test adds a third, declared-public claim.
//! Stage 2 reads beliefs AS the synthesis owner (`Viewer::resolve`).
//!
//! # Test graph
//!
//! Builds a small, deterministic in-memory graph backed by `MockEdgeProvider`:
//!
//!     seed (aaaa…) ─SUPPORTS─▶ bbbb…
//!                  ─SUPPORTS─▶ cccc…  (test-inserted)
//!                                │
//!                                └─SUPPORTS─▶ bbbb…  (revisit, dropped)
//!
//! With max_hops ≥ 2 and a `ConstantEmbedder` returning identical embeddings
//! for every claim (cosine = 1.0 ≥ relevance_prune), BFS visits all three. The
//! test asserts `claim_ids.len() > seeds.len()` (the plan's traversal-progress
//! check) along with one belief-interval per claim and durable persistence to
//! both `syntheses.subgraph_snapshot` and `synthesis_claim_membership`.
mod support;
use support::TestDb;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use epigraph_core::TenancyDecl;
use epigraph_db::Viewer;
use episcience_core::synthesis::traversal::{EdgeProvider, EdgeType, TraversalConfig};
use episcience_db::SynthesisPipeline;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use epigraph_cli::enrichment::llm_client::{LlmError, LlmProvider};
use epigraph_embeddings::errors::EmbeddingError;
use epigraph_embeddings::service::{EmbeddingService, SimilarClaim, TokenUsage};

// ──────────────────────────────────────────────────────────────────────────────
// Test doubles
// ──────────────────────────────────────────────────────────────────────────────

/// Embedder that returns the same constant vector for every claim. Cosine vs
/// `query_embedding = [1.0; 8]` is 1.0, so every neighbour passes the
/// `relevance_prune = 0.3` default cutoff.
#[derive(Debug)]
struct ConstantEmbedder {
    embedding: Vec<f32>,
}

impl Default for ConstantEmbedder {
    fn default() -> Self {
        Self {
            embedding: vec![1.0; 8],
        }
    }
}

#[async_trait]
impl EmbeddingService for ConstantEmbedder {
    async fn generate(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(self.embedding.clone())
    }

    async fn batch_generate(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Ok(vec![self.embedding.clone()])
    }

    async fn store(&self, _claim_id: Uuid, _embedding: &[f32]) -> Result<(), EmbeddingError> {
        Ok(())
    }

    async fn get(&self, _claim_id: Uuid) -> Result<Vec<f32>, EmbeddingError> {
        Ok(self.embedding.clone())
    }

    async fn similar(
        &self,
        _embedding: &[f32],
        _k: usize,
        _min_similarity: f32,
    ) -> Result<Vec<SimilarClaim>, EmbeddingError> {
        Ok(vec![])
    }

    fn dimension(&self) -> usize {
        self.embedding.len()
    }

    fn token_usage(&self) -> TokenUsage {
        TokenUsage::default()
    }

    fn reset_token_usage(&self) {}

    async fn health_check(&self) -> Result<(), EmbeddingError> {
        Ok(())
    }

    async fn generate_query(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(self.embedding.clone())
    }
}

#[derive(Debug, Default)]
struct MockLlmClient;

#[async_trait]
impl LlmProvider for MockLlmClient {
    fn name(&self) -> &str {
        "mock"
    }

    fn is_active(&self) -> bool {
        true
    }

    async fn complete_json(&self, _prompt: &str) -> Result<serde_json::Value, LlmError> {
        Ok(serde_json::json!({}))
    }

    fn model_name(&self) -> &str {
        "mock"
    }
}

/// Edge provider backed by a fixed in-memory adjacency map.
struct MockEdgeProvider {
    adj: HashMap<Uuid, Vec<(Uuid, EdgeType)>>,
}

#[async_trait]
impl EdgeProvider for MockEdgeProvider {
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

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Pre-seeded test agent in `epigraph_dev_synthesis` (P5 validation).
fn test_agent_id() -> Uuid {
    "f3951e28-9356-42b6-9c80-27dd9f01b19d".parse().unwrap()
}

/// Pre-seeded `aaaa…` and `bbbb…` claims (Phase 0 fixtures).
fn seed_claim_a() -> Uuid {
    "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".parse().unwrap()
}
fn seed_claim_b() -> Uuid {
    "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".parse().unwrap()
}

/// A declared PUBLIC claim authored by the seed agent, owned by its personal
/// group (never an undeclared insert).
async fn insert_public_claim(pool: &PgPool, content: &str) -> Uuid {
    let pg: Uuid = sqlx::query_scalar("SELECT public.epigraph_ensure_personal_group($1)")
        .bind(test_agent_id())
        .fetch_one(pool)
        .await
        .expect("seed agent personal group");
    support::claim(pool, test_agent_id(), content, 0.7, TenancyDecl::public(pg)).await
}

async fn insert_pending_synthesis(pool: &PgPool, owner: Uuid) -> Uuid {
    let synthesis_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO syntheses
         (id, query, agent_id, status, subgraph_snapshot,
          clustering_method, llm_provider, llm_model,
          content_hash, visibility, owner_group_id)
         VALUES ($1, 'stage2 test', $2, 'pending', '{}'::jsonb,
                 'signed_louvain', 'mock', 'mock',
                 $3, 'group', public.epigraph_ensure_personal_group($2))",
    )
    .bind(synthesis_id)
    .bind(owner)
    .bind(&[0u8; 32][..])
    .execute(pool)
    .await
    .expect("insert synthesis row");
    synthesis_id
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

/// Stage 2 should: traverse beyond the seeds, populate one belief interval per
/// surviving claim, and persist both the snapshot JSON and the membership rows
/// in a single transaction.
#[tokio::test]
async fn stage2_traverse_persists_snapshot_and_membership() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();

    let claim_c = insert_public_claim(&pool, "stage2 test claim — origami at 70C").await;
    let synthesis_id = insert_pending_synthesis(&pool, test_agent_id()).await;
    let viewer = Viewer::resolve(&pool, test_agent_id())
        .await
        .expect("resolve owner");

    // Build the in-memory graph: aaaa → {bbbb, cccc}; cccc → {bbbb}.
    let mut adj: HashMap<Uuid, Vec<(Uuid, EdgeType)>> = HashMap::new();
    adj.insert(
        seed_claim_a(),
        vec![
            (seed_claim_b(), EdgeType::Supports),
            (claim_c, EdgeType::Supports),
        ],
    );
    adj.insert(claim_c, vec![(seed_claim_b(), EdgeType::Supports)]);

    let pipeline = SynthesisPipeline::new(
        pool.clone(),
        Arc::new(ConstantEmbedder::default()),
        MockLlmClient,
        MockEdgeProvider { adj },
        // query_embedding cosines to 1.0 against ConstantEmbedder.get(_).
        vec![1.0; 8],
        // cost_budget — Stage 2 makes no LLM calls; spec default.
        20,
    );

    let cfg = TraversalConfig::default(); // max_hops=2, prune=0.3, max_size=500
    let seeds = vec![seed_claim_a()];

    let snapshot = pipeline
        .stage2_traverse(&viewer, synthesis_id, seeds.clone(), &cfg)
        .await
        .expect("stage2_traverse should succeed against pre-seeded DB");

    // ── In-memory snapshot assertions ──────────────────────────────────────
    assert!(
        snapshot.claim_ids.len() > seeds.len(),
        "expected traversal to discover neighbours beyond the seed; \
         got claim_ids.len()={} (seeds.len()={})",
        snapshot.claim_ids.len(),
        seeds.len()
    );
    assert_eq!(
        snapshot.belief_intervals.len(),
        snapshot.claim_ids.len(),
        "every claim_id should have one belief-interval entry"
    );
    // All three known claims should be present.
    let cids: std::collections::HashSet<Uuid> = snapshot.claim_ids.iter().copied().collect();
    assert!(cids.contains(&seed_claim_a()), "missing seed aaaa…");
    assert!(cids.contains(&seed_claim_b()), "missing neighbour bbbb…");
    assert!(cids.contains(&claim_c), "missing test claim cccc…");

    // Pre-seeded aaaa… has truth_value=0.8 → unframed cached belief == 0.8.
    let bi_a = snapshot
        .belief_intervals
        .iter()
        .find(|b| b.claim_id == seed_claim_a())
        .expect("belief for aaaa…");
    assert!(!bi_a.framed, "unframed path should be framed=false");
    assert!(
        (bi_a.belief - 0.8).abs() < 1e-9,
        "expected aaaa belief=0.8, got {}",
        bi_a.belief
    );

    // ── Durable persistence assertions ─────────────────────────────────────
    // 1. synthesis_claim_membership row count matches.
    let row_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM synthesis_claim_membership WHERE synthesis_id = $1",
    )
    .bind(synthesis_id)
    .fetch_one(&pool)
    .await
    .expect("count membership");
    assert_eq!(
        row_count as usize,
        snapshot.claim_ids.len(),
        "membership row count must match snapshot.claim_ids.len()"
    );

    // 2. subgraph_snapshot is non-trivial JSON (was '{}'::jsonb before).
    let snap_row = sqlx::query("SELECT subgraph_snapshot FROM syntheses WHERE id = $1")
        .bind(synthesis_id)
        .fetch_one(&pool)
        .await
        .expect("fetch synthesis row");
    let snap_json: serde_json::Value = snap_row.get("subgraph_snapshot");
    assert!(
        snap_json.is_object(),
        "subgraph_snapshot must be a JSON object, got {snap_json:?}"
    );
    let claim_ids_json = snap_json
        .get("claim_ids")
        .and_then(|v| v.as_array())
        .expect("subgraph_snapshot.claim_ids should be an array");
    assert_eq!(
        claim_ids_json.len(),
        snapshot.claim_ids.len(),
        "persisted claim_ids length must match in-memory snapshot"
    );
    let bi_json = snap_json
        .get("belief_intervals")
        .and_then(|v| v.as_array())
        .expect("subgraph_snapshot.belief_intervals should be an array");
    assert_eq!(
        bi_json.len(),
        snapshot.claim_ids.len(),
        "persisted belief_intervals length must match claim_ids length"
    );
}

/// Stage 2 fails CLOSED when the traversal reaches a claim the synthesis
/// owner cannot read: the belief lookup runs as the owner, the kernel reports
/// the claim as not found, the stage errors, and NO snapshot or membership row
/// is written (so the invisible claim never enters the synthesis).
///
/// Kills: a belief lookup that ignores the owner's viewer (a bypass or an
/// unfiltered read would score the invisible claim and persist it).
#[tokio::test]
async fn stage2_traverse_refuses_a_claim_the_owner_cannot_read() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let h1 = support::principal(&pool, "h1").await;
    let h2 = support::principal(&pool, "h2").await;
    let hidden = support::claim(
        &pool,
        h1.agent,
        "stage2 claim private to H1",
        0.9,
        TenancyDecl::group(h1.personal_group),
    )
    .await;
    assert_eq!(
        support::claim_pair(&pool, hidden).await.0,
        "group",
        "fixture must really be group-owned"
    );
    let synthesis_id = insert_pending_synthesis(&pool, h2.agent).await;
    let viewer = Viewer::resolve(&pool, h2.agent).await.expect("resolve h2");

    let mut adj: HashMap<Uuid, Vec<(Uuid, EdgeType)>> = HashMap::new();
    adj.insert(seed_claim_a(), vec![(hidden, EdgeType::Supports)]);
    let pipeline = SynthesisPipeline::new(
        pool.clone(),
        Arc::new(ConstantEmbedder::default()),
        MockLlmClient,
        MockEdgeProvider { adj },
        vec![1.0; 8],
        20,
    );
    let r = pipeline
        .stage2_traverse(
            &viewer,
            synthesis_id,
            vec![seed_claim_a()],
            &TraversalConfig::default(),
        )
        .await;
    assert!(
        r.is_err(),
        "an invisible claim must fail the stage, got {r:?}"
    );

    let members: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM synthesis_claim_membership WHERE synthesis_id = $1",
    )
    .bind(synthesis_id)
    .fetch_one(&pool)
    .await
    .expect("count membership");
    assert_eq!(members, 0, "no membership row may be written");
    let snap: serde_json::Value =
        sqlx::query_scalar("SELECT subgraph_snapshot FROM syntheses WHERE id = $1")
            .bind(synthesis_id)
            .fetch_one(&pool)
            .await
            .expect("snapshot");
    assert_eq!(
        snap,
        serde_json::json!({}),
        "the snapshot must stay untouched"
    );

    // The same traversal as H1 (who can read the claim) succeeds and keeps it,
    // so the failure above is the viewer's doing.
    let h1_synthesis = insert_pending_synthesis(&pool, h1.agent).await;
    let h1_viewer = Viewer::resolve(&pool, h1.agent).await.expect("resolve h1");
    let snapshot = pipeline
        .stage2_traverse(
            &h1_viewer,
            h1_synthesis,
            vec![seed_claim_a()],
            &TraversalConfig::default(),
        )
        .await
        .expect("H1 can read every claim on the path");
    assert!(snapshot.claim_ids.contains(&hidden));
}
