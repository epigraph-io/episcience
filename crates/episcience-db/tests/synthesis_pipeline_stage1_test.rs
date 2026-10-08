//! Stage 1 (`stage1_seed`) integration tests for `SynthesisPipeline`.
//!
//! # DB strategy
//!
//! Each test runs on its own clone of the E1 template (`TestDb::fresh`): the
//! kernel schema at the pinned rev (built by the kernel's `epigraph-migrate`)
//! plus EpiScience's, seeded by scripts/ci-seed.sql with two PUBLIC
//! `origami melts at ...` claims (truth 0.8 / 0.85).
//!
//! # Embedding strategy
//!
//! The tests use an `ErroringEmbedder` whose `generate_query` always returns
//! `Err`, forcing `recall::recall` onto its text-search fallback (`ILIKE` on
//! `claims.content`, filtered by the viewer), so a unique sentinel string
//! returns exactly zero rows.
//!
//! # Viewer
//!
//! Stage 1 recalls AS the synthesis owner. `stage1_seed_excludes_claims_the_owner_cannot_read`
//! (T-J5s) pins that another principal's group-owned claim never seeds this
//! owner's synthesis on recall's text-search leg;
//! `stage1_semantic_seed_excludes_claims_the_owner_cannot_read` pins the same
//! on the embedding (nearest-neighbour) leg that production seeds through.
//!
//! # Theme seed (`stage1_seed_theme`)
//!
//! The wiki-article seed reads a theme's members through the kernel's
//! viewer-spliced `ClaimThemeRepository` reads, then picks seeds by MMR.
//! `stage1_theme_seed_reads_only_members_the_viewer_can_read` pins that the
//! viewer bounds those reads at pipeline level, with no handler and no seed
//! filter behind it; `stage1_theme_seed_member_read_spends_the_viewer` pins
//! the member-list read on its own; and
//! `stage1_theme_seed_without_query_embedding_refuses_before_any_read` pins
//! the fail-closed refusal when the embedder produced no query vector.
mod support;
use support::TestDb;

use std::sync::Arc;

use async_trait::async_trait;
use epigraph_core::TenancyDecl;
use epigraph_db::Viewer;
use episcience_core::synthesis::errors::SynthesisError;
use episcience_core::synthesis::traversal::{EdgeProvider, EdgeType};
use episcience_db::SynthesisPipeline;
use sqlx::PgPool;
use uuid::Uuid;

use epigraph_cli::enrichment::llm_client::{LlmError, LlmProvider};
use epigraph_embeddings::errors::EmbeddingError;
use epigraph_embeddings::service::{EmbeddingService, SimilarClaim, TokenUsage};

// ──────────────────────────────────────────────────────────────────────────────
// Test doubles
// ──────────────────────────────────────────────────────────────────────────────

/// An embedder whose `generate_query` always errors. Forces `recall::recall`
/// onto the text-search fallback path so test outcomes are predictable.
#[derive(Debug, Default)]
struct ErroringEmbedder;

#[async_trait]
impl EmbeddingService for ErroringEmbedder {
    async fn generate(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Err(EmbeddingError::ApiError {
            message: "test stub: generate disabled".to_string(),
            status_code: None,
        })
    }

    async fn batch_generate(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Err(EmbeddingError::ApiError {
            message: "test stub: batch_generate disabled".to_string(),
            status_code: None,
        })
    }

    async fn store(&self, _claim_id: Uuid, _embedding: &[f32]) -> Result<(), EmbeddingError> {
        Err(EmbeddingError::ApiError {
            message: "test stub: store disabled".to_string(),
            status_code: None,
        })
    }

    async fn get(&self, claim_id: Uuid) -> Result<Vec<f32>, EmbeddingError> {
        Err(EmbeddingError::NotFound { claim_id })
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
        1536
    }

    fn token_usage(&self) -> TokenUsage {
        TokenUsage::default()
    }

    fn reset_token_usage(&self) {}

    async fn health_check(&self) -> Result<(), EmbeddingError> {
        Ok(())
    }

    async fn generate_query(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        // Force recall::recall onto its text-search fallback path.
        Err(EmbeddingError::ApiError {
            message: "test stub: generate_query disabled — use text fallback".to_string(),
            status_code: None,
        })
    }
}

/// An embedder whose `generate_query` returns [`fixed_vector`], so
/// `recall::recall` takes its EMBEDDING leg (`search_by_embedding_current`
/// plus the per-hit belief and claim reads), as production does. Every other
/// method errors: stage 1 only calls `generate_query`.
#[derive(Debug, Default)]
struct FixedQueryEmbedder;

/// A unit vector in the claims' embedding dimension.
fn fixed_vector() -> Vec<f32> {
    let mut v = vec![0.0_f32; 1536];
    v[0] = 1.0;
    v
}

#[async_trait]
impl EmbeddingService for FixedQueryEmbedder {
    async fn generate(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Err(EmbeddingError::ApiError {
            message: "test stub: generate disabled".to_string(),
            status_code: None,
        })
    }

    async fn batch_generate(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Err(EmbeddingError::ApiError {
            message: "test stub: batch_generate disabled".to_string(),
            status_code: None,
        })
    }

    async fn store(&self, _claim_id: Uuid, _embedding: &[f32]) -> Result<(), EmbeddingError> {
        Err(EmbeddingError::ApiError {
            message: "test stub: store disabled".to_string(),
            status_code: None,
        })
    }

    async fn get(&self, claim_id: Uuid) -> Result<Vec<f32>, EmbeddingError> {
        Err(EmbeddingError::NotFound { claim_id })
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
        1536
    }

    fn token_usage(&self) -> TokenUsage {
        TokenUsage::default()
    }

    fn reset_token_usage(&self) {}

    async fn health_check(&self) -> Result<(), EmbeddingError> {
        Ok(())
    }

    async fn generate_query(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(fixed_vector())
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

struct MockEdgeProvider;

#[async_trait]
impl EdgeProvider for MockEdgeProvider {
    async fn neighbors(&self, _claim: Uuid, _types: &[EdgeType]) -> Vec<(Uuid, EdgeType)> {
        vec![]
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// The seed agent (scripts/ci-seed.sql) that authored the two public claims.
const SEED_AGENT: Uuid = Uuid::from_u128(0xf3951e28_9356_42b6_9c80_27dd9f01b19d);

fn build_pipeline(pool: PgPool) -> SynthesisPipeline<MockLlmClient, MockEdgeProvider> {
    build_pipeline_with(pool, Arc::new(ErroringEmbedder))
}

fn build_pipeline_with(
    pool: PgPool,
    embedder: Arc<dyn EmbeddingService>,
) -> SynthesisPipeline<MockLlmClient, MockEdgeProvider> {
    // `stage1_seed` doesn't read query_embedding; pass an empty vec.
    build_pipeline_with_query(pool, embedder, vec![])
}

/// A pipeline carrying `query_embedding`, which `stage1_seed_theme` ranks
/// theme members against.
fn build_pipeline_with_query(
    pool: PgPool,
    embedder: Arc<dyn EmbeddingService>,
    query_embedding: Vec<f32>,
) -> SynthesisPipeline<MockLlmClient, MockEdgeProvider> {
    SynthesisPipeline::new(
        pool,
        embedder,
        MockLlmClient,
        MockEdgeProvider,
        query_embedding,
        // cost_budget — irrelevant to Stage 1 (no LLM calls); spec default.
        20,
    )
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

/// Stage 1 should return seed UUIDs for a query that matches pre-seeded claims.
///
/// The `epigraph_dev_synthesis` DB is pre-seeded with two claims containing
/// "origami" in their content. With `ErroringEmbedder` forcing the text-search
/// fallback, `query="origami"` runs `ILIKE '%origami%'` and matches both rows.
#[tokio::test]
async fn stage1_seed_returns_recall_results() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let viewer = Viewer::resolve(&pool, SEED_AGENT).await.expect("resolve");
    let pipeline = build_pipeline(pool);

    let seeds = pipeline
        .stage1_seed(&viewer, "origami", 50, 0.5)
        .await
        .expect("stage1_seed should succeed against pre-seeded DB");

    assert!(
        !seeds.is_empty(),
        "expected at least one seed for query 'origami', got {} results",
        seeds.len()
    );
    // Sanity: every returned id parses as a Uuid (the pipeline already does this,
    // but assert it here so the test fails loudly if the contract changes).
    for id in &seeds {
        assert_ne!(*id, Uuid::nil(), "seed id should be non-nil");
    }
}

/// Stage 1 should return `EmptyResult` when recall has no matches.
///
/// Forces text-search fallback (via `ErroringEmbedder`) and queries for a
/// sentinel string that cannot occur in claim content. The ILIKE query
/// returns zero rows, so `recall::recall` returns `Ok(vec![])`, and
/// `stage1_seed` maps that to `SynthesisError::EmptyResult`.
#[tokio::test]
async fn stage1_seed_empty_returns_error() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let viewer = Viewer::resolve(&pool, SEED_AGENT).await.expect("resolve");
    let pipeline = build_pipeline(pool);

    let r = pipeline
        .stage1_seed(&viewer, "never-occurring-string-xyz123", 50, 0.5)
        .await;

    assert!(
        matches!(r, Err(SynthesisError::EmptyResult)),
        "expected EmptyResult for sentinel query, got {:?}",
        r
    );
}

/// T-J5s: Stage 1 seeds for H2's synthesis never include H1's GROUP-owned
/// claim, while H1's own synthesis does seed from it (so the fixture is
/// findable and the exclusion is the viewer's doing, not a miss).
///
/// Kills: passing an unrestricted / wrong viewer to `recall` (for example the
/// seed agent or a bypass viewer instead of the synthesis owner), or dropping
/// the viewer from the stage entirely.
#[tokio::test]
async fn stage1_seed_excludes_claims_the_owner_cannot_read() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let h1 = support::principal(&pool, "h1").await;
    let h2 = support::principal(&pool, "h2").await;
    let private = support::claim(
        &pool,
        h1.agent,
        "origami folding notebook entry kept inside H1 personal group",
        0.9,
        TenancyDecl::group(h1.personal_group),
    )
    .await;
    assert_eq!(
        support::claim_pair(&pool, private).await,
        ("group".to_string(), h1.personal_group),
        "fixture must really be group-owned, or the exclusion below is vacuous"
    );

    let h1_viewer = Viewer::resolve(&pool, h1.agent).await.expect("resolve h1");
    let h2_viewer = Viewer::resolve(&pool, h2.agent).await.expect("resolve h2");
    let pipeline = build_pipeline(pool);

    let h2_seeds = pipeline
        .stage1_seed(&h2_viewer, "origami", 50, 0.5)
        .await
        .expect("public origami claims still seed H2's synthesis");
    assert!(
        !h2_seeds.contains(&private),
        "H2's seeds must not include H1's group-owned claim"
    );
    assert!(
        h2_seeds.contains(&Uuid::from_u128(0xaaaaaaaa_aaaa_aaaa_aaaa_aaaaaaaaaaaa)),
        "H2 still sees the public seed claim"
    );

    let h1_seeds = pipeline
        .stage1_seed(&h1_viewer, "origami", 50, 0.5)
        .await
        .expect("H1 seeds");
    assert!(
        h1_seeds.contains(&private),
        "H1's own synthesis seeds from H1's group-owned claim"
    );
}

/// T-J5s on the EMBEDDING leg. H1's group-owned claim and the public seed
/// claim carry the query's own embedding, and the query text matches no claim
/// content, so recall's text-search fallback would return nothing: every seed
/// below comes from the nearest-neighbour search. H2's seeds hold the public
/// claim and never H1's group claim; H1's hold it.
///
/// Kills: a wrong viewer passed to `recall` on the embedding leg (the
/// EpiScience call site in `stage1_seed`), which the text-leg test above
/// cannot see because its embedder always errors. It does NOT kill a mutant
/// inside the pinned kernel's `recall`: that leg filters by viewer twice (the
/// ANN query and the per-hit claim read), so an unrestricted viewer at either
/// one alone leaves the result unchanged, and the per-hit belief read only
/// rescores hits already in hand. That each kernel repo function spends its
/// viewer is held by the kernel's own source lint
/// (`epigraph-db/tests/visibility_lint.rs`), not by this test.
#[tokio::test]
async fn stage1_semantic_seed_excludes_claims_the_owner_cannot_read() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let h1 = support::principal(&pool, "h1").await;
    let h2 = support::principal(&pool, "h2").await;
    let private = support::claim(
        &pool,
        h1.agent,
        "semantic-leg fixture kept inside H1 personal group",
        0.9,
        TenancyDecl::group(h1.personal_group),
    )
    .await;
    assert_eq!(
        support::claim_pair(&pool, private).await,
        ("group".to_string(), h1.personal_group),
        "fixture must really be group-owned, or the exclusion below is vacuous"
    );
    let public_seed = Uuid::from_u128(0xaaaaaaaa_aaaa_aaaa_aaaa_aaaaaaaaaaaa);
    let literal = format!(
        "[{}]",
        fixed_vector()
            .iter()
            .map(f32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    );
    let n = sqlx::query("UPDATE public.claims SET embedding = $1::vector WHERE id = ANY($2)")
        .bind(&literal)
        .bind(vec![private, public_seed])
        .execute(&pool)
        .await
        .expect("store fixture embeddings")
        .rows_affected();
    assert_eq!(n, 2, "both fixture claims must carry the query embedding");

    // Matches no claim content: a text-leg seed is impossible.
    let query = "zq-semantic-leg-only-7f3c";
    let h1_viewer = Viewer::resolve(&pool, h1.agent).await.expect("resolve h1");
    let h2_viewer = Viewer::resolve(&pool, h2.agent).await.expect("resolve h2");
    let pipeline = build_pipeline_with(pool, Arc::new(FixedQueryEmbedder));

    let h2_seeds = pipeline
        .stage1_seed(&h2_viewer, query, 50, 0.5)
        .await
        .expect("the public claim seeds H2's synthesis through the embedding leg");
    assert!(
        h2_seeds.contains(&public_seed),
        "H2 sees the public claim on the embedding leg"
    );
    assert!(
        !h2_seeds.contains(&private),
        "H2's seeds must not include H1's group-owned claim"
    );

    let h1_seeds = pipeline
        .stage1_seed(&h1_viewer, query, 50, 0.5)
        .await
        .expect("H1 seeds");
    assert!(
        h1_seeds.contains(&private),
        "H1's own synthesis seeds from H1's group-owned claim on the embedding leg"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Theme seed (`stage1_seed_theme`)
// ──────────────────────────────────────────────────────────────────────────────

/// Unit vector on axis `k` in the claims' embedding dimension.
fn axis(k: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; 1536];
    v[k] = 1.0;
    v
}

fn pgvector_literal(v: &[f32]) -> String {
    format!(
        "[{}]",
        v.iter().map(f32::to_string).collect::<Vec<_>>().join(",")
    )
}

/// A theme row keyed on `properties`, as the wiki fixtures elsewhere build it.
async fn insert_theme(pool: &PgPool, label: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO public.claim_themes (label, description, properties) \
         VALUES ($1, '', jsonb_build_object('cluster_run_id', gen_random_uuid(), 'cluster_id', 7)) \
         RETURNING id",
    )
    .bind(label)
    .fetch_one(pool)
    .await
    .expect("insert theme")
}

/// A declared claim that is a member of `theme` and carries a stored
/// 1536-dim `embedding` (the column the theme read ranks on).
async fn theme_member(
    pool: &PgPool,
    author: Uuid,
    content: &str,
    decl: TenancyDecl,
    embedding: &[f32],
    theme: Uuid,
) -> Uuid {
    let id = support::claim(pool, author, content, 0.9, decl).await;
    let n =
        sqlx::query("UPDATE public.claims SET embedding = $2::vector, theme_id = $3 WHERE id = $1")
            .bind(id)
            .bind(pgvector_literal(embedding))
            .bind(theme)
            .execute(pool)
            .await
            .expect("store fixture embedding + theme")
            .rows_affected();
    assert_eq!(n, 1, "fixture claim must carry its embedding and theme");
    id
}

async fn theme_seeds(
    pipeline: &SynthesisPipeline<MockLlmClient, MockEdgeProvider>,
    viewer: &Viewer,
    theme: Uuid,
) -> std::collections::BTreeSet<Uuid> {
    pipeline
        .stage1_seed_theme(viewer, theme)
        .await
        .expect("the public member always seeds")
        .into_iter()
        .collect()
}

/// Theme T holds a public member A, a member F private to a team only the
/// outsider is in, and a member G private to the actor's personal group. The
/// theme seed, run straight on the pipeline (no handler, so no seed filter
/// behind it) and on the clone's superuser pool (so row security hides
/// nothing and only the viewer predicate the kernel splices into the
/// theme reads decides), returns exactly the members each viewer can read:
///
/// | viewer   | seeds  |
/// |----------|--------|
/// | actor    | {A, G} |
/// | outsider | {A, F} |
/// | stranger | {A}    |
///
/// The outsider row is the non-vacuity control: F is reachable through this
/// read, so its absence elsewhere is the viewer's doing, not a fixture slip.
/// The actor row's G is the control against dropping every group claim.
///
/// Kills: a bypass or wrong viewer passed to BOTH kernel reads in
/// `stage1_seed_theme` (the member list `claims_in_themes_at_dim` and the
/// per-member `get_claim_embedding_str`), or a theme read that drops group
/// claims wholesale. It does NOT see a wrong viewer at the per-member read
/// alone: the member list has already removed F by then, so that read never
/// runs on F (no leak to observe).
/// `stage1_theme_seed_member_read_spends_the_viewer` pins the member-list read
/// on its own.
#[tokio::test]
async fn stage1_theme_seed_reads_only_members_the_viewer_can_read() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let actor = support::principal(&pool, "theme-actor").await;
    let outsider = support::principal(&pool, "theme-outsider").await;
    let stranger = support::principal(&pool, "theme-stranger").await;
    let author = support::principal(&pool, "theme-author").await;
    let team = support::team_group(&pool, &outsider, &[]).await;
    let theme = insert_theme(&pool, "Origami folding").await;

    // A leans toward the query axis so the ranking is deterministic; A, F, G
    // sit on distinct axes so near-duplicate suppression drops none of them.
    let mut a_emb = axis(1);
    a_emb[0] = 0.05;
    let a = theme_member(
        &pool,
        author.agent,
        "theme fixture A, public",
        TenancyDecl::public(author.personal_group),
        &a_emb,
        theme,
    )
    .await;
    let f = theme_member(
        &pool,
        outsider.agent,
        "theme fixture F, private to the outsider's team",
        TenancyDecl::group(team),
        &axis(6),
        theme,
    )
    .await;
    let g = theme_member(
        &pool,
        actor.agent,
        "theme fixture G, private to the actor's personal group",
        TenancyDecl::group(actor.personal_group),
        &axis(5),
        theme,
    )
    .await;
    assert_eq!(
        support::claim_pair(&pool, f).await,
        ("group".to_string(), team),
        "F must really be group-owned, or the exclusions below are vacuous"
    );
    assert_eq!(
        support::claim_pair(&pool, g).await,
        ("group".to_string(), actor.personal_group),
        "G must really be group-owned, or the positive control is vacuous"
    );
    assert_eq!(
        support::claim_pair(&pool, a).await.0,
        "public",
        "A is the public member every viewer reads"
    );

    let actor_v = Viewer::resolve(&pool, actor.agent).await.expect("actor");
    let outsider_v = Viewer::resolve(&pool, outsider.agent)
        .await
        .expect("outsider");
    let stranger_v = Viewer::resolve(&pool, stranger.agent)
        .await
        .expect("stranger");
    // ErroringEmbedder: the theme seed ranks on STORED claim embeddings read
    // through the kernel, never through `EmbeddingService::get`.
    let pipeline = build_pipeline_with_query(pool, Arc::new(ErroringEmbedder), axis(0));

    assert_eq!(
        theme_seeds(&pipeline, &actor_v, theme).await,
        [a, g].into_iter().collect(),
        "the actor seeds A and its own group's G, never the other team's F"
    );
    assert_eq!(
        theme_seeds(&pipeline, &outsider_v, theme).await,
        [a, f].into_iter().collect(),
        "the outsider seeds A and its team's F (F is findable), never the actor's G"
    );
    assert_eq!(
        theme_seeds(&pipeline, &stranger_v, theme).await,
        [a].into_iter().collect(),
        "a viewer in neither group seeds only the public member"
    );
}

/// A theme whose ONLY member is private to the outsider's team: a stranger's
/// theme seed finds no members and fails `Validation` naming the theme and
/// saying no member was readable, while the outsider's seeds that member
/// (non-vacuity).
///
/// Kills: a bypass or wrong viewer at the member-list read
/// (`claims_in_themes_at_dim`) alone. The private row would then come back,
/// the viewer-spliced per-member embedding read would return nothing for it,
/// and the seed would fail with the "no usable stored claim embedding"
/// reason instead, which the matrix test above cannot tell apart from
/// correct code. Also kills a bare `EmptyResult` ("seed recall returned no
/// claims for query"), which tells the operator neither the theme nor why.
#[tokio::test]
async fn stage1_theme_seed_member_read_spends_the_viewer() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let outsider = support::principal(&pool, "theme-outsider").await;
    let stranger = support::principal(&pool, "theme-stranger").await;
    let team = support::team_group(&pool, &outsider, &[]).await;
    let theme = insert_theme(&pool, "Private-only theme").await;
    let p = theme_member(
        &pool,
        outsider.agent,
        "theme fixture P, the theme's only member, private to the outsider's team",
        TenancyDecl::group(team),
        &axis(1),
        theme,
    )
    .await;
    assert_eq!(
        support::claim_pair(&pool, p).await,
        ("group".to_string(), team),
        "P must really be group-owned, or the exclusion below is vacuous"
    );

    let outsider_v = Viewer::resolve(&pool, outsider.agent)
        .await
        .expect("outsider");
    let stranger_v = Viewer::resolve(&pool, stranger.agent)
        .await
        .expect("stranger");
    let pipeline = build_pipeline_with_query(pool, Arc::new(ErroringEmbedder), axis(0));

    match pipeline.stage1_seed_theme(&stranger_v, theme).await {
        Err(SynthesisError::Validation(m)) => {
            assert!(
                m.contains(&theme.to_string()),
                "the reason names the theme: {m}"
            );
            assert!(
                m.contains("no member") && m.contains("readable"),
                "the reason says no member was readable: {m}"
            );
            assert!(
                !m.contains("embedding"),
                "a stranger's member read must return no rows, not rows without \
                 embeddings: {m}"
            );
        }
        other => panic!("a stranger's member read must return no rows: got {other:?}"),
    }
    let seeds = pipeline
        .stage1_seed_theme(&outsider_v, theme)
        .await
        .expect("the outsider reads its team's member");
    assert_eq!(seeds, vec![p], "P is findable by a viewer in its group");
}

/// Without a query embedding (the embedder failed) the theme seed refuses
/// with a `Validation` naming the query embedding, BEFORE any database read:
/// the pipeline runs on a closed pool, on which any read fails `Db` (the
/// second call, with an embedding, proves that).
///
/// Kills: dropping the empty-embedding guard (the read then ranks against an
/// empty vector), or moving it after the kernel read.
#[tokio::test]
async fn stage1_theme_seed_without_query_embedding_refuses_before_any_read() {
    let db = TestDb::fresh().await;
    let viewer = Viewer::resolve(&db.admin, SEED_AGENT)
        .await
        .expect("resolve");
    let closed = PgPool::connect_with(db.admin_options())
        .await
        .expect("second pool on the clone");
    closed.close().await;
    let theme = Uuid::now_v7();

    let without = build_pipeline_with_query(closed.clone(), Arc::new(ErroringEmbedder), vec![]);
    match without.stage1_seed_theme(&viewer, theme).await {
        Err(SynthesisError::Validation(m)) => assert!(
            m.contains("query embedding"),
            "the refusal names the missing query embedding: {m}"
        ),
        other => panic!("expected Validation before any read, got {other:?}"),
    }

    let with = build_pipeline_with_query(closed, Arc::new(ErroringEmbedder), axis(0));
    let r = with.stage1_seed_theme(&viewer, theme).await;
    assert!(
        matches!(r, Err(SynthesisError::Db(_))),
        "control: a read on the closed pool fails Db, got {r:?}"
    );
}
