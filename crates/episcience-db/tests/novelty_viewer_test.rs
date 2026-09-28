//! T-R5s: the novelty backends read only what the candidate synthesis' OWNER
//! can read.
//!
//! - Internal backend: prior syntheses come only from those the owner can read
//!   under the synthesis read rule (public, its own, or shared with it).
//! - Paper backend: DOI-labelled kernel claims are read AS the owner (the
//!   kernel's `/* {VISIBILITY:c} */` splice).
mod support;
use support::TestDb;

use std::sync::Arc;

use epigraph_core::TenancyDecl;
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_core::synthesis::novelty::NoveltyBackend;
use episcience_db::synthesis::novelty_backend_internal::InternalNoveltyBackend;
use episcience_db::synthesis::novelty_backend_paper::PaperNoveltyBackend;
use episcience_db::SynthesisEmbeddingsRepository;
use sqlx::PgPool;
use uuid::Uuid;

/// The public seed claim every principal can read (scripts/ci-seed.sql).
const SHARED_MEMBER: Uuid = Uuid::from_u128(0xaaaaaaaa_aaaa_aaaa_aaaa_aaaaaaaaaaaa);

fn embedder() -> Arc<dyn EmbeddingService> {
    Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)))
}

/// Insert a synthesis row owned by `owner`. `complete` rows carry a narrative,
/// a membership row on [`SHARED_MEMBER`] and a narrative embedding, so they
/// qualify as priors.
async fn synthesis(pool: &PgPool, owner: Uuid, visibility: &str, complete: bool) -> Uuid {
    let id = Uuid::now_v7();
    let (status, narrative) = if complete {
        ("complete", Some("prior narrative about origami"))
    } else {
        ("pending", None)
    };
    sqlx::query(
        "INSERT INTO syntheses (id, query, agent_id, status, narrative, completed_at, \
             subgraph_snapshot, clustering_method, llm_provider, llm_model, content_hash, visibility, \
             owner_group_id) \
         VALUES ($1, 'novelty viewer test', $2, $3, $4, \
             CASE WHEN $3 = 'complete' THEN now() END, '{}'::jsonb, 'signed_louvain', \
             'mock', 'mock', $5, $6, public.epigraph_ensure_personal_group($2))",
    )
    .bind(id)
    .bind(owner)
    .bind(status)
    .bind(narrative)
    .bind(&[0u8; 32][..])
    .bind(visibility)
    .execute(pool)
    .await
    .expect("insert synthesis");
    if complete {
        sqlx::query(
            "INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)",
        )
        .bind(id)
        .bind(SHARED_MEMBER)
        .execute(pool)
        .await
        .expect("membership");
        let v = embedder()
            .generate("prior narrative about origami")
            .await
            .expect("embed");
        SynthesisEmbeddingsRepository::upsert(pool, id, &v, "mock", "narrative_head")
            .await
            .expect("embedding");
    }
    id
}

fn neighbour_ids(score: &episcience_core::synthesis::novelty::NoveltyScore) -> Vec<Uuid> {
    score.neighbours.iter().map(|n| n.synthesis_id).collect()
}

// T-R5s (internal). Kills: dropping the owner-readability predicate from
// `find_priors_with_overlap` (H2's candidate would be scored against, and
// name, H1's private synthesis).
#[tokio::test]
async fn internal_priors_exclude_syntheses_the_owner_cannot_read() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let h1 = support::principal(&pool, "h1").await;
    let h2 = support::principal(&pool, "h2").await;
    let h1_private = synthesis(&pool, h1.agent, "group", true).await;
    let h1_public = synthesis(&pool, h1.agent, "public", true).await;
    let h2_candidate = synthesis(&pool, h2.agent, "group", false).await;
    let h1_candidate = synthesis(&pool, h1.agent, "group", false).await;
    let backend = InternalNoveltyBackend {
        pool: pool.clone(),
        embedder: embedder(),
    };

    let h2_score = backend
        .score(h2_candidate, "candidate narrative", &[SHARED_MEMBER])
        .await
        .expect("score H2");
    let h2_priors = neighbour_ids(&h2_score);
    assert!(
        h2_priors.contains(&h1_public),
        "a public prior is comparable"
    );
    assert!(
        !h2_priors.contains(&h1_private),
        "H1's private synthesis must not be a prior of H2's synthesis"
    );

    let h1_priors = neighbour_ids(
        &backend
            .score(h1_candidate, "candidate narrative", &[SHARED_MEMBER])
            .await
            .expect("score H1"),
    );
    assert!(h1_priors.contains(&h1_private) && h1_priors.contains(&h1_public));
}

// T-R5s (paper). A DOI-labelled claim private to H1, embedded identically to
// the candidate narrative, drives H1's DOI similarity to ~1 but is invisible
// to H2's candidate. Kills: dropping the splice (or its bind) from
// `find_top_doi_claim_similarity`.
#[tokio::test]
async fn paper_backend_reads_doi_claims_as_the_owner() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let h1 = support::principal(&pool, "h1").await;
    let h2 = support::principal(&pool, "h2").await;
    let narrative = "a narrative that matches the private DOI claim exactly";
    let doi_claim = support::claim(
        &pool,
        h1.agent,
        "a DOI-labelled finding private to H1",
        0.9,
        TenancyDecl::group(h1.personal_group),
    )
    .await;
    assert_eq!(support::claim_pair(&pool, doi_claim).await.0, "group");
    let v = embedder().generate(narrative).await.expect("embed");
    let literal = format!(
        "[{}]",
        v.iter().map(f32::to_string).collect::<Vec<_>>().join(",")
    );
    sqlx::query("UPDATE claims SET labels = ARRAY['doi'], embedding = $2::vector WHERE id = $1")
        .bind(doi_claim)
        .bind(literal)
        .execute(&pool)
        .await
        .expect("label + embed the DOI claim");

    let backend = PaperNoveltyBackend {
        pool: pool.clone(),
        embedder: embedder(),
    };
    let h2_candidate = synthesis(&pool, h2.agent, "group", false).await;
    let h1_candidate = synthesis(&pool, h1.agent, "group", false).await;

    let h2_score = backend
        .score(h2_candidate, narrative, &[])
        .await
        .expect("H2");
    assert!(
        h2_score.rationale.contains("top_doi_similarity 0.000"),
        "H2 must not see H1's private DOI claim: {}",
        h2_score.rationale
    );
    let h1_score = backend
        .score(h1_candidate, narrative, &[])
        .await
        .expect("H1");
    assert!(
        h1_score.rationale.contains("top_doi_similarity 1.000"),
        "H1's own DOI claim is compared: {}",
        h1_score.rationale
    );
}
