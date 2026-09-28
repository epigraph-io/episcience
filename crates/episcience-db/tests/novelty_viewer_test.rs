//! T-R5: the novelty backends read only what the candidate synthesis' JOB
//! PRINCIPAL can read, within the candidate's own audience.
//!
//! - Internal backend: prior syntheses come only from those the principal can
//!   read (public, or owned by one of its groups) AND the candidate's
//!   audience can read.
//! - Paper backend: DOI-labelled kernel claims are read AS the principal (the
//!   kernel's `/* {VISIBILITY:c} */` splice), within the same audience.
//!
//! The backends run on the connection they are handed. On the worker that is
//! the stage transaction stamped as the principal (T-R5 on the worker login
//! below: row security AND the splice); the other cases use the clone's
//! superuser connection, where the splice and the audience bound are all that
//! restrict the read, so each of those is tested on its own.
mod support;
use support::TestDb;

use std::sync::Arc;

use epigraph_core::TenancyDecl;
use epigraph_db::{ScopedPool, ScopedPoolOptions, SessionGucMode, Viewer};
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_db::synthesis::novelty::{
    NoveltyBackend, NoveltyCandidate, NoveltyError, NoveltyScore,
};
use episcience_db::synthesis::novelty_backend_internal::InternalNoveltyBackend;
use episcience_db::synthesis::novelty_backend_paper::PaperNoveltyBackend;
use episcience_db::synthesis::publish::narrative_head;
use episcience_db::SynthesisEmbeddingsRepository;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

/// The public seed claim every principal can read (scripts/ci-seed.sql).
const SHARED_MEMBER: Uuid = Uuid::from_u128(0xaaaaaaaa_aaaa_aaaa_aaaa_aaaaaaaaaaaa);

fn embedder() -> Arc<dyn EmbeddingService> {
    Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)))
}

/// Score `candidate` with `backend` on `conn` as `reader`, computing the
/// embeddings the way the worker does (the head always; the full narrative
/// only for a backend that asks).
async fn score(
    backend: &dyn NoveltyBackend,
    conn: &mut PgConnection,
    reader: &Viewer,
    candidate: Uuid,
    narrative: &str,
    members: &[Uuid],
) -> Result<NoveltyScore, NoveltyError> {
    let e = embedder();
    let head = e
        .generate(narrative_head(narrative))
        .await
        .expect("embed head");
    let full = if backend.wants_narrative_embedding() {
        Some(e.generate(narrative).await.expect("embed narrative"))
    } else {
        None
    };
    backend
        .score(
            conn,
            reader,
            &NoveltyCandidate {
                id: candidate,
                member_ids: members,
                head_embedding: &head,
                narrative_embedding: full.as_deref(),
            },
        )
        .await
}

/// [`score`] on the clone's superuser connection, reading as `principal`.
async fn score_on_admin(
    pool: &PgPool,
    backend: &dyn NoveltyBackend,
    principal: Uuid,
    candidate: Uuid,
    narrative: &str,
    members: &[Uuid],
) -> NoveltyScore {
    let reader = support::viewer_of(pool, principal).await;
    let mut conn = pool.acquire().await.expect("admin connection");
    score(backend, &mut conn, &reader, candidate, narrative, members)
        .await
        .expect("score")
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
    if !complete {
        // A candidate is scored inside its job, which acts as its principal.
        sqlx::query(
            "INSERT INTO synthesis_jobs (id, payload, state, principal_id) \
             VALUES ($1, '{}'::jsonb, 'running', $2)",
        )
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .expect("candidate job");
    }
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
    let backend = InternalNoveltyBackend;

    let h2_score = score_on_admin(
        &pool,
        &backend,
        h2.agent,
        h2_candidate,
        "candidate narrative",
        &[SHARED_MEMBER],
    )
    .await;
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
        &score_on_admin(
            &pool,
            &backend,
            h1.agent,
            h1_candidate,
            "candidate narrative",
            &[SHARED_MEMBER],
        )
        .await,
    );
    assert!(h1_priors.contains(&h1_private) && h1_priors.contains(&h1_public));
}

// T-R5 on the worker's application login (review E1g finding 2: the worker
// used to score on its UNSTAMPED engine pool, where row security hides every
// `synthesis_jobs` row, so it found no prior at all and every synthesis
// scored 1.0). On the stage transaction stamped as H2 (the candidate's job
// principal), H2's GROUP candidate is compared with H2's own group prior and
// with H1's public prior, never with H1's group prior. Control: the same
// fixture on the superuser connection gives the same set, so the stamped
// result is neither empty by accident nor wider. And on the worker's
// unstamped pool (the old wiring) nothing is found: the stamped session is
// what makes the priors visible. Kills: novelty handed an unstamped pool on
// the worker (no prior: the finding), and a stamped read that hides the
// principal's own group prior or shows another group's.
#[tokio::test]
async fn internal_priors_on_the_worker_login_follow_the_stamped_principal() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let h1 = support::principal(&pool, "h1").await;
    let h2 = support::principal(&pool, "h2").await;
    let h1_group = synthesis(&pool, h1.agent, "group", true).await;
    let h1_public = synthesis(&pool, h1.agent, "public", true).await;
    let h2_group = synthesis(&pool, h2.agent, "group", true).await;
    let h2_candidate = synthesis(&pool, h2.agent, "group", false).await;
    let expected =
        |p: &[Uuid]| p.contains(&h2_group) && p.contains(&h1_public) && !p.contains(&h1_group);

    let control = neighbour_ids(
        &score_on_admin(
            &pool,
            &InternalNoveltyBackend,
            h2.agent,
            h2_candidate,
            "candidate narrative",
            &[SHARED_MEMBER],
        )
        .await,
    );
    assert!(expected(&control), "control: {control:?}");

    let worker = ScopedPool::connect_with_options(
        &db.login_url(support::WORKER_LOGIN),
        SessionGucMode::Session,
        ScopedPoolOptions::default(),
    )
    .await
    .expect("stamped pool on the worker login");
    let v2 = support::viewer_of(&pool, h2.agent).await;
    let mut tx = worker.begin_as(&v2).await.expect("begin_as H2");
    let (user, privileged): (String, bool) = sqlx::query_as(
        "SELECT session_user::text, (SELECT rolsuper OR rolbypassrls FROM pg_roles \
          WHERE rolname = session_user)",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("session");
    assert_eq!(user, "episcience_worker");
    assert!(!privileged);
    let stamped = score(
        &InternalNoveltyBackend,
        &mut tx,
        &v2,
        h2_candidate,
        "candidate narrative",
        &[SHARED_MEMBER],
    )
    .await
    .expect("score on the stamped worker session");
    let stamped_ids = neighbour_ids(&stamped);
    assert!(expected(&stamped_ids), "stamped: {stamped_ids:?}");
    assert!(stamped.score < 1.0, "a prior was found: {}", stamped.score);
    drop(tx);

    let unstamped = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(db.login_options(support::WORKER_LOGIN))
        .await
        .expect("the worker login, unstamped");
    let mut conn = unstamped.acquire().await.expect("unstamped connection");
    let none = score(
        &InternalNoveltyBackend,
        &mut conn,
        &v2,
        h2_candidate,
        "candidate narrative",
        &[SHARED_MEMBER],
    )
    .await
    .expect("score on the unstamped pool");
    assert!(
        none.neighbours.is_empty() && none.score == 1.0,
        "the unstamped pool sees no candidate job row: {:?}",
        neighbour_ids(&none)
    );
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

    let backend = PaperNoveltyBackend;
    let h2_candidate = synthesis(&pool, h2.agent, "group", false).await;
    let h1_candidate = synthesis(&pool, h1.agent, "group", false).await;

    let h2_score = score_on_admin(&pool, &backend, h2.agent, h2_candidate, narrative, &[]).await;
    assert!(
        h2_score.rationale.contains("top_doi_similarity 0.000"),
        "H2 must not see H1's private DOI claim: {}",
        h2_score.rationale
    );
    let h1_score = score_on_admin(&pool, &backend, h1.agent, h1_candidate, narrative, &[]).await;
    assert!(
        h1_score.rationale.contains("top_doi_similarity 1.000"),
        "H1's own DOI claim is compared: {}",
        h1_score.rationale
    );
    // E1d review R13: a PUBLIC candidate of H1 is outside the group claim's
    // audience, although H1 (its principal) can read the claim. Kills:
    // dropping the audience bound from the DOI query.
    let h1_public_candidate = synthesis(&pool, h1.agent, "public", false).await;
    let public_score = score_on_admin(
        &pool,
        &backend,
        h1.agent,
        h1_public_candidate,
        narrative,
        &[],
    )
    .await;
    assert!(
        public_score.rationale.contains("top_doi_similarity 0.000"),
        "a public candidate is never scored against a group claim: {}",
        public_score.rationale
    );
}

/// A prior (complete, sharing SHARED_MEMBER, embedded) or a candidate (with a
/// job acting as `principal`), authored by `author`, owned by `owner` with
/// `visibility`.
async fn owned_synthesis(
    pool: &PgPool,
    author: Uuid,
    owner: Uuid,
    visibility: &str,
    candidate_principal: Option<Uuid>,
) -> Uuid {
    let id = Uuid::now_v7();
    let complete = candidate_principal.is_none();
    sqlx::query(
        "INSERT INTO syntheses (id, query, agent_id, status, narrative, completed_at, \
             subgraph_snapshot, clustering_method, llm_provider, llm_model, content_hash, visibility, \
             owner_group_id) \
         VALUES ($1, 'novelty audience test', $2, $3, $4, CASE WHEN $3 = 'complete' THEN now() END, \
             '{}'::jsonb, 'signed_louvain', 'mock', 'mock', $5, $6, $7)",
    )
    .bind(id)
    .bind(author)
    .bind(if complete { "complete" } else { "pending" })
    .bind(complete.then_some("prior narrative about origami"))
    .bind(&[0u8; 32][..])
    .bind(visibility)
    .bind(owner)
    .execute(pool)
    .await
    .expect("insert synthesis");
    match candidate_principal {
        Some(p) => {
            sqlx::query(
                "INSERT INTO synthesis_jobs (id, payload, state, principal_id) \
                 VALUES ($1, '{}'::jsonb, 'running', $2)",
            )
            .bind(id)
            .bind(p)
            .execute(pool)
            .await
            .expect("candidate job");
        }
        None => {
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
    }
    id
}

/// E1d review R13 (D-S9): novelty priors are read as the candidate's JOB
/// principal, not its author, and only within the candidate's audience.
/// (1) A candidate AUTHORED by H2 whose job acts as H1 sees H1's group
/// prior (H2 could not). (2) A PUBLIC candidate is never compared with (nor
/// names) a group prior its principal can read. (3) A `group(T)` candidate
/// is compared with T's prior but not with its principal's personal-group
/// prior (T's readers cannot see it). Kills: reading as `syntheses.agent_id`
/// (a reader other than the job principal is refused), or dropping the
/// audience bound.
#[tokio::test]
async fn novelty_reads_as_the_job_principal_within_the_candidates_audience() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let h1 = support::principal(&pool, "h1").await;
    let h2 = support::principal(&pool, "h2").await;
    let t = support::team_group(&pool, &h1, &[]).await;
    let h1_private = owned_synthesis(&pool, h1.agent, h1.personal_group, "group", None).await;
    let team_prior = owned_synthesis(&pool, h1.agent, t, "group", None).await;
    let public_prior = owned_synthesis(&pool, h1.agent, h1.personal_group, "public", None).await;
    let priors = |candidate: Uuid| {
        let pool = &pool;
        async move {
            neighbour_ids(
                &score_on_admin(
                    pool,
                    &InternalNoveltyBackend,
                    h1.agent,
                    candidate,
                    "candidate narrative",
                    &[SHARED_MEMBER],
                )
                .await,
            )
        }
    };

    let by_h2_as_h1 =
        owned_synthesis(&pool, h2.agent, h1.personal_group, "group", Some(h1.agent)).await;
    let p = priors(by_h2_as_h1).await;
    assert!(
        p.contains(&h1_private),
        "(1) read as the job principal: {p:?}"
    );
    // ... and never as its AUTHOR: a reader that is not the job's principal
    // is refused, so no score can be computed through the wrong eyes.
    let as_author = support::viewer_of(&pool, h2.agent).await;
    let mut conn = pool.acquire().await.expect("admin connection");
    let refused = score(
        &InternalNoveltyBackend,
        &mut conn,
        &as_author,
        by_h2_as_h1,
        "candidate narrative",
        &[SHARED_MEMBER],
    )
    .await
    .expect_err("a reader other than the job principal is refused");
    assert!(refused.to_string().contains("job principal"), "{refused}");
    drop(conn);

    let public_candidate =
        owned_synthesis(&pool, h1.agent, h1.personal_group, "public", Some(h1.agent)).await;
    let p = priors(public_candidate).await;
    assert!(p.contains(&public_prior), "(2) {p:?}");
    assert!(
        !p.contains(&h1_private) && !p.contains(&team_prior),
        "(2) a public candidate names no group prior: {p:?}"
    );

    let team_candidate = owned_synthesis(&pool, h1.agent, t, "group", Some(h1.agent)).await;
    let p = priors(team_candidate).await;
    assert!(
        p.contains(&team_prior) && p.contains(&public_prior),
        "(3) {p:?}"
    );
    assert!(
        !p.contains(&h1_private),
        "(3) the principal's personal prior is outside T's audience: {p:?}"
    );
}
