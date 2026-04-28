//! Stage 2 (`stage2_traverse`) integration tests for `SynthesisPipeline`.
//!
//! # DB strategy
//!
//! Targets the live `epigraph_dev_synthesis` database. We rely on:
//!   - upstream `claims` rows (Phase 0 pre-seeded `aaaa…` and `bbbb…` claims;
//!     this test pre-inserts a third short-lived `cccc…` row so traversal can
//!     exercise the multi-hop path).
//!   - upstream `agents` rows (the pre-seeded `f3951e28-…` test service agent).
//!   - synthesis-side tables (`syntheses`, `synthesis_claim_membership`),
//!     applied to `epigraph_dev_synthesis` from `migrations/synthesis/`.
//!
//! Why not `#[sqlx::test(migrations = ...)]`? Stage 2 calls
//! `epigraph_engine::belief_query::get_belief`, which queries upstream tables
//! (`claims`, `mass_functions`, `frames`) that aren't in the local
//! `migrations/` tree. The live DB has both schemas applied.
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

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use episcience_core::synthesis::traversal::{EdgeProvider, EdgeType, TraversalConfig};
use episcience_db::SynthesisPipeline;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use epigraph_cli::enrichment::llm_client::{LlmClient, LlmError};
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
impl LlmClient for MockLlmClient {
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

async fn connect_epigraph() -> PgPool {
    PgPool::connect("postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_dev_synthesis")
        .await
        .expect("connect to epigraph_dev_synthesis")
}

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

/// Insert a short-lived test claim with a stable id. Returns the id used.
async fn insert_test_claim(pool: &PgPool, id: Uuid, content: &str) -> Uuid {
    let agent_id = test_agent_id();
    // Deterministic dummy 32-byte content hash (real hash not required here —
    // schema only enforces 32-byte length, not blake3 of `content`).
    let content_hash = [0xCCu8; 32];
    sqlx::query(
        "INSERT INTO claims (id, content, content_hash, truth_value, agent_id)
         VALUES ($1, $2, $3, 0.7, $4)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(id)
    .bind(content)
    .bind(&content_hash[..])
    .bind(agent_id)
    .execute(pool)
    .await
    .expect("insert test claim");
    id
}

async fn delete_test_claim(pool: &PgPool, id: Uuid) {
    let _ = sqlx::query("DELETE FROM claims WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

/// Stage 2 should: traverse beyond the seeds, populate one belief interval per
/// surviving claim, and persist both the snapshot JSON and the membership rows
/// in a single transaction.
#[tokio::test]
async fn stage2_traverse_persists_snapshot_and_membership() {
    let pool = connect_epigraph().await;

    // Synthetic third claim id — must be deterministic so we can clean up even
    // if a previous run aborted mid-test.
    let claim_c: Uuid = "cccccccc-cccc-cccc-cccc-cccccccccccc".parse().unwrap();
    insert_test_claim(&pool, claim_c, "stage2 test claim — origami at 70C").await;

    let synthesis_id = Uuid::now_v7();
    // Pre-create the synthesis row so save_snapshot_tx has something to UPDATE.
    sqlx::query(
        "INSERT INTO syntheses
         (id, query, agent_id, status, subgraph_snapshot,
          clustering_method, llm_provider, llm_model,
          content_hash, visibility)
         VALUES ($1, 'stage2 test', $2, 'pending', '{}'::jsonb,
                 'signed_louvain', 'mock', 'mock',
                 $3, 'private')",
    )
    .bind(synthesis_id)
    .bind(test_agent_id())
    .bind(&[0u8; 32][..])
    .execute(&pool)
    .await
    .expect("insert synthesis row");

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
        .stage2_traverse(synthesis_id, seeds.clone(), &cfg)
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

    // ── Cleanup ────────────────────────────────────────────────────────────
    // Delete in dependency order: membership → synthesis → test claim.
    sqlx::query("DELETE FROM synthesis_claim_membership WHERE synthesis_id = $1")
        .bind(synthesis_id)
        .execute(&pool)
        .await
        .expect("cleanup membership");
    sqlx::query("DELETE FROM syntheses WHERE id = $1")
        .bind(synthesis_id)
        .execute(&pool)
        .await
        .expect("cleanup synthesis");
    delete_test_claim(&pool, claim_c).await;
}
