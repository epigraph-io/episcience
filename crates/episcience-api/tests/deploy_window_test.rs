//! The E1d binary on the 5034 schema: the deploy window between the expand
//! step and the contract step, when the E1d binary is installed but 5035's
//! row guards do not exist yet. The application must hold the tenancy rules
//! itself there (the guards are its backstop from 5035 on), so each case runs
//! the REAL REST router over a kernel-only clone migrated to 5034 exactly.
//!
//! Cast: H1 and H2 (personal groups), a bystander with no membership.
#[path = "../../episcience-db/tests/support/mod.rs"]
mod testdb;

use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION};
use axum::http::StatusCode;
use axum_test::TestServer;
use epigraph_core::TenancyDecl;
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use episcience_db::ledger;
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;
use testdb::TestDb;
use uuid::Uuid;

#[path = "support/token.rs"]
mod token;
use token::{jwt_secret_bytes, mint_test_jwt};

fn bearer(agent: Uuid) -> (HeaderName, HeaderValue) {
    (
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", mint_test_jwt(agent))).expect("bearer"),
    )
}

async fn at_5034() -> TestDb {
    let db = TestDb::fresh_kernel_only().await;
    let mut c = ledger::connect_with(db.admin_options())
        .await
        .expect("ledger connection");
    ledger::run_to(&mut c, Some(ledger::TENANCY_EXPAND_VERSION))
        .await
        .expect("episcience migrations up to 5034");
    db
}

async fn server(pool: PgPool, blobs: &std::path::Path) -> TestServer {
    let embedder: Arc<dyn EmbeddingService> =
        Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    TestServer::new(episcience_api::create_router(ElnState {
        db: testdb::app_db_for(&pool).await,
        blob_dir: blobs.to_path_buf(),
        jwt_config: Arc::new(JwtConfig::from_secret(&jwt_secret_bytes())),
        max_upload_bytes: 1024 * 1024,
        embedder,
    }))
    .expect("TestServer")
}

/// A declared synthesis written on the admin pool.
async fn synthesis(pool: &PgPool, author: Uuid, vis: &str, owner: Uuid, query: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO syntheses (id, query, agent_id, status, subgraph_snapshot, clustering_method, \
             llm_provider, llm_model, content_hash, visibility, owner_group_id) \
         VALUES ($1, $2, $3, 'pending', '{}'::jsonb, 'signed_louvain', 'p', 'm', \
                 decode(repeat('00', 32), 'hex'), $4, $5)",
    )
    .bind(id)
    .bind(query)
    .bind(author)
    .bind(vis)
    .bind(owner)
    .execute(pool)
    .await
    .expect("synthesis fixture");
    id
}

async fn visibility_of(pool: &PgPool, id: Uuid) -> String {
    sqlx::query_scalar("SELECT visibility::text FROM syntheses WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// R5 (the application half): at 5034, a PUBLIC refinement of a GROUP
/// synthesis is stored `group` (a bystander cannot read it, so the parent's
/// query does not leak), and so is a synthesis asked public with a GROUP
/// prerequisite; a synthesis with no non-public input stays public. Kills:
/// child_ownership honouring the requested visibility under a non-public
/// parent, or narrow_for_prerequisites removed.
#[tokio::test]
async fn a_public_synthesis_with_a_group_input_is_born_group_before_the_guards_exist() {
    let db = at_5034().await;
    let a = &db.admin;
    let h1 = testdb::principal(a, "h1").await;
    let bystander = testdb::principal(a, "bystander").await;
    let blobs = tempfile::TempDir::new().unwrap();
    let srv = server(a.clone(), blobs.path()).await;
    let parent = synthesis(a, h1.agent, "group", h1.personal_group, "a secret query").await;

    let (hn, hv) = bearer(h1.agent);
    let resp = srv
        .post(&format!("/api/v1/eln/syntheses/{parent}/refine"))
        .add_header(hn, hv)
        .json(&json!({"visibility": "public"}))
        .await;
    assert_eq!(resp.status_code(), StatusCode::ACCEPTED, "{}", resp.text());
    let child: Uuid = resp.json::<serde_json::Value>()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(visibility_of(a, child).await, "group");
    let (hn, hv) = bearer(bystander.agent);
    let resp = srv
        .get(&format!("/api/v1/eln/syntheses/{child}"))
        .add_header(hn, hv)
        .await;
    assert_eq!(resp.status_code(), StatusCode::NOT_FOUND);
    assert!(!resp.text().contains("a secret query"));

    for (prereqs, want) in [(vec![parent], "group"), (vec![], "public")] {
        let (hn, hv) = bearer(h1.agent);
        let resp = srv
            .post("/api/v1/eln/syntheses")
            .add_header(hn, hv)
            .json(&json!({"query": "q", "prereq_synthesis_ids": prereqs, "visibility": "public"}))
            .await;
        assert_eq!(resp.status_code(), StatusCode::ACCEPTED, "{}", resp.text());
        let id: Uuid = resp.json::<serde_json::Value>()["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(visibility_of(a, id).await, want, "prereqs {prereqs:?}");
    }
}

/// R6 (the application half): at 5034, widening a synthesis that cites a
/// GROUP claim is refused (403, the widening guard's own words) and the row
/// is unchanged; one whose members are all public widens. Kills: the
/// publishability check removed from `set_visibility_as` (the widen would
/// succeed until 5035 existed).
#[tokio::test]
async fn widening_a_synthesis_with_a_group_member_is_refused_before_the_guards_exist() {
    let db = at_5034().await;
    let a = &db.admin;
    let h1 = testdb::principal(a, "h1").await;
    let g = h1.personal_group;
    let blobs = tempfile::TempDir::new().unwrap();
    let srv = server(a.clone(), blobs.path()).await;
    let own = testdb::claim(
        a,
        h1.agent,
        &format!("own {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::group(g),
    )
    .await;
    let public = testdb::claim(
        a,
        h1.agent,
        &format!("pub {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::public(g),
    )
    .await;
    let blocked = synthesis(a, h1.agent, "group", g, "q").await;
    let open = synthesis(a, h1.agent, "group", g, "q").await;
    for (s, c) in [(blocked, own), (open, public)] {
        sqlx::query(
            "INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)",
        )
        .bind(s)
        .bind(c)
        .execute(a)
        .await
        .unwrap();
    }
    for (s, want) in [
        (blocked, StatusCode::FORBIDDEN),
        (open, StatusCode::NO_CONTENT),
    ] {
        let (hn, hv) = bearer(h1.agent);
        let resp = srv
            .patch(&format!("/api/v1/eln/syntheses/{s}/visibility"))
            .add_header(hn, hv)
            .json(&json!({"visibility": "public"}))
            .await;
        assert_eq!(resp.status_code(), want, "{}", resp.text());
    }
    assert_eq!(visibility_of(a, blocked).await, "group");
    assert_eq!(visibility_of(a, open).await, "public");
}

/// R7, on the E1g runtime: an observation whose text equals ANOTHER group's
/// GROUP claim never links or returns that claim, both at 5034 and on the
/// full schema. The request runs on a session stamped as the caller, so the
/// kernel's content dedup cannot SEE the other group's claim (kernel claims
/// row security): the caller gets its OWN claim (public, in its default
/// group, as for any observation on a public sample), and the response says
/// nothing about the other claim's existence (the E1d-era 403 did: that
/// residual of the global dedup is closed). Kills: the request path
/// reverting to an unstamped or privileged session (the dedup would return
/// and link the other group's claim, as it did before R7), and an
/// observation claim declared into another group.
#[tokio::test]
async fn an_observation_never_links_another_groups_claim_found_by_content() {
    for full in [false, true] {
        let db = if full {
            TestDb::fresh().await
        } else {
            at_5034().await
        };
        let a = &db.admin;
        let h1 = testdb::principal(a, "h1").await;
        let h2 = testdb::principal(a, "h2").await;
        let blobs = tempfile::TempDir::new().unwrap();
        let srv = server(a.clone(), blobs.path()).await;
        let secret = format!("a private finding {}", Uuid::new_v4());
        let theirs = testdb::claim(
            a,
            h1.agent,
            &secret,
            0.8,
            TenancyDecl::group(h1.personal_group),
        )
        .await;
        let sample = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO samples (id, name, sample_type, prepared_by, content_hash, owner_group_id, visibility) \
             VALUES ($1, 's', 'chemical', $2, decode(md5($1::text) || md5($1::text), 'hex'), $3, 'public')",
        )
        .bind(sample)
        .bind(h2.agent)
        .bind(h2.personal_group)
        .execute(a)
        .await
        .unwrap();
        let observe = |content: String| {
            let (hn, hv) = bearer(h2.agent);
            srv.post(&format!("/api/v1/eln/samples/{sample}/observations"))
                .add_header(hn, hv)
                .json(&json!({"content": content, "agent_id": h2.agent}))
        };
        let resp = observe(secret.clone()).await;
        assert_eq!(
            resp.status_code(),
            StatusCode::OK,
            "full={full}: {}",
            resp.text()
        );
        assert!(!resp.text().contains(&theirs.to_string()));
        let mine: Uuid = resp.json::<serde_json::Value>()["claim_id"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .expect("a claim id");
        assert_ne!(mine, theirs, "full={full}: the caller's own claim");
        let links: Vec<Uuid> =
            sqlx::query_scalar("SELECT claim_id FROM sample_claims WHERE sample_id = $1")
                .bind(sample)
                .fetch_all(a)
                .await
                .unwrap();
        assert_eq!(links, vec![mine], "full={full}: only the caller's claim");
        assert_eq!(
            testdb::claim_pair(a, mine).await,
            ("public".to_string(), h2.personal_group),
            "full={full}: public, in the caller's own group"
        );
        assert_eq!(
            testdb::claim_pair(a, theirs).await,
            ("group".to_string(), h1.personal_group),
            "full={full}: the other group's claim is untouched"
        );
        let resp = observe(format!("fresh {}", Uuid::new_v4())).await;
        assert_eq!(
            resp.status_code(),
            StatusCode::OK,
            "full={full}: {}",
            resp.text()
        );
    }
}

/// E1d review R15, the helper half: in the window a legacy job has no
/// principal (the re-own sets it later). `refinement_principal` answers
/// `None` for it (never a guessed identity: the payload's or the row's
/// author is a legacy shared agent there), and once the job has a principal
/// it returns exactly that one. Kills: the helper reading the payload's
/// agent (a `coalesce` with `payload->>'agent_id'`, or a fallback inside the
/// helper). The CALLER's fallback (`unwrap_or(payload.agent_id)` in the
/// handler) is killed by the handler-level test
/// `synthesis_job_handler_test::a_rejected_legacy_job_without_a_principal_spawns_nothing_and_does_not_retry`.
#[tokio::test]
async fn a_legacy_job_without_a_principal_spawns_no_refinement() {
    let db = at_5034().await;
    let a = &db.admin;
    let h1 = testdb::principal(a, "h1").await;
    let s = synthesis(a, h1.agent, "group", h1.personal_group, "q").await;
    sqlx::query(
        "INSERT INTO synthesis_jobs (id, payload, state) \
         VALUES ($1, jsonb_build_object('agent_id', $2), 'running')",
    )
    .bind(s)
    .bind(h1.agent)
    .execute(a)
    .await
    .expect("a legacy job row with no principal");
    let mut conn = a.acquire().await.unwrap();
    let r = episcience_api::jobs::synthesis_job::refinement_principal(&mut conn, s)
        .await
        .expect("reading the principal succeeds");
    assert_eq!(r, None, "no principal: no refinement");
    sqlx::query("UPDATE synthesis_jobs SET principal_id = $2 WHERE id = $1")
        .bind(s)
        .bind(h1.agent)
        .execute(a)
        .await
        .unwrap();
    let r = episcience_api::jobs::synthesis_job::refinement_principal(&mut conn, s)
        .await
        .expect("reading the principal succeeds");
    assert_eq!(r, Some(h1.agent));
}

/// The `Extensions` rmcp hands a tool once `call_tool` authorized `agent`
/// with read + write scope (the production path inserts exactly this).
async fn mcp_caller(
    server: &episcience_api::mcp::EpiscienceServer,
    agent: Uuid,
) -> rmcp::model::Extensions {
    let mut ext = rmcp::model::Extensions::new();
    server
        .attach_caller(
            &mut ext,
            episcience_api::middleware::AuthContext {
                agent_id: agent,
                client_id: Uuid::new_v4(),
                owner_id: None,
                client_type: "human".to_string(),
                scopes: vec!["claims:read".to_string(), "claims:write".to_string()],
            },
        )
        .await
        .expect("the caller resolves (call_tool's own step)");
    ext
}

/// E1d delta D2 (R5, the MCP half): at 5034, the MCP `synthesize` tool asked
/// for a PUBLIC synthesis with a GROUP prerequisite stores it `group`; the
/// same request without the prerequisite stays `public` (so the narrowing is
/// caused by the prerequisite, not by a blanket default). Kills: removing
/// `narrow_for_prerequisites` from `mcp/synthesize.rs::handle` (the REST half
/// is pinned by the test above; until 5035 nothing else narrows the row).
#[tokio::test]
async fn mcp_synthesize_with_a_group_prerequisite_is_born_group_before_the_guards_exist() {
    let db = at_5034().await;
    let a = &db.admin;
    let h1 = testdb::principal(a, "h1").await;
    let prereq = synthesis(a, h1.agent, "group", h1.personal_group, "a secret prereq").await;
    let blobs = tempfile::TempDir::new().unwrap();
    let embedder: Arc<dyn EmbeddingService> =
        Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    let mcp = episcience_api::mcp::EpiscienceServer::new(
        testdb::app_db_for(a).await,
        embedder,
        blobs.path().to_path_buf(),
        1024 * 1024,
    );
    for (prereqs, want) in [(vec![prereq], "group"), (vec![], "public")] {
        let result = mcp
            .synthesize(
                rmcp::handler::server::wrapper::Parameters(
                    episcience_api::mcp::synthesize::SynthesizeArgs {
                        query: "q".to_string(),
                        traversal_config: None,
                        parent_synthesis_id: None,
                        prereq_synthesis_ids: prereqs.clone(),
                        wait_for_completion: false,
                        timeout_seconds: 0,
                        visibility: "public".to_string(),
                        owner_group_id: None,
                    },
                ),
                mcp_caller(&mcp, h1.agent).await,
            )
            .await
            .expect("synthesize tool call");
        let body: serde_json::Value = match &result.content.first().expect("content").raw {
            rmcp::model::RawContent::Text(t) => serde_json::from_str(&t.text).expect("json"),
            other => panic!("unexpected content {other:?}"),
        };
        let id: Uuid = body["synthesis_id"].as_str().unwrap().parse().unwrap();
        assert_eq!(visibility_of(a, id).await, want, "prereqs {prereqs:?}");
    }
}

/// E1d delta D2 (R5/R6, the parent and prerequisite arms of the widening
/// check): at 5034, widening a refinement whose PARENT is `group`, or a
/// synthesis whose PREREQUISITE is `group`, is refused with the widening
/// rule's words and the row stays `group`; a refinement of a PUBLIC parent
/// widens (so the refusal is caused by the parent's visibility). None of
/// them has a member claim, so only the parent/prerequisite arms decide.
/// Kills: the parent arm of `SynthesisRepository::set_visibility_as` made
/// always true (`AND (true OR s.parent_synthesis_id IS NULL ...`), and the
/// prerequisite arm made always true.
#[tokio::test]
async fn widening_a_synthesis_with_a_group_parent_or_prerequisite_is_refused_before_the_guards_exist(
) {
    let db = at_5034().await;
    let a = &db.admin;
    let h1 = testdb::principal(a, "h1").await;
    let g = h1.personal_group;
    let blobs = tempfile::TempDir::new().unwrap();
    let srv = server(a.clone(), blobs.path()).await;
    let group_parent = synthesis(a, h1.agent, "group", g, "group parent").await;
    let public_parent = synthesis(a, h1.agent, "public", g, "public parent").await;
    let under_group = synthesis(a, h1.agent, "group", g, "q").await;
    let under_public = synthesis(a, h1.agent, "group", g, "q").await;
    let with_prereq = synthesis(a, h1.agent, "group", g, "q").await;
    for (child, parent) in [(under_group, group_parent), (under_public, public_parent)] {
        sqlx::query("UPDATE syntheses SET parent_synthesis_id = $2 WHERE id = $1")
            .bind(child)
            .bind(parent)
            .execute(a)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE syntheses SET prereq_synthesis_ids = ARRAY[$2]::uuid[] WHERE id = $1")
        .bind(with_prereq)
        .bind(group_parent)
        .execute(a)
        .await
        .unwrap();
    for (s, refused) in [
        (under_group, true),
        (with_prereq, true),
        (under_public, false),
    ] {
        let (hn, hv) = bearer(h1.agent);
        let resp = srv
            .patch(&format!("/api/v1/eln/syntheses/{s}/visibility"))
            .add_header(hn, hv)
            .json(&json!({"visibility": "public"}))
            .await;
        if refused {
            assert_eq!(resp.status_code(), StatusCode::FORBIDDEN, "{}", resp.text());
            assert!(
                resp.text()
                    .contains("a member claim, its parent or a prerequisite is not public"),
                "refused by the widening rule, not by authority: {}",
                resp.text()
            );
            assert_eq!(visibility_of(a, s).await, "group");
        } else {
            assert_eq!(
                resp.status_code(),
                StatusCode::NO_CONTENT,
                "{}",
                resp.text()
            );
            assert_eq!(visibility_of(a, s).await, "public");
        }
    }
}

/// E1d delta D2 (R6, the public-sample arm of the attach rule): an
/// observation on the caller's OWN PUBLIC sample whose text equals the
/// caller's OWN group claim (content dedup hands that claim back, and it is
/// owned by the sample's group, so the owner arm passes) is refused with the
/// "public sample" words, links nothing and returns no claim id, both at
/// 5034 and on the full schema; the same text on the caller's GROUP sample
/// links (so the refusal is caused by the sample's visibility). Kills:
/// disabling `if sample_vis.as_deref() != Some("group")` in
/// `SampleRepository::add_observation` (at 5034 the link would be written).
#[tokio::test]
async fn an_observation_on_a_public_sample_never_links_the_owners_group_claim() {
    for full in [false, true] {
        let db = if full {
            TestDb::fresh().await
        } else {
            at_5034().await
        };
        let a = &db.admin;
        let h1 = testdb::principal(a, "h1").await;
        let g = h1.personal_group;
        let blobs = tempfile::TempDir::new().unwrap();
        let srv = server(a.clone(), blobs.path()).await;
        let secret = format!("an own group finding {}", Uuid::new_v4());
        let own = testdb::claim(a, h1.agent, &secret, 0.8, TenancyDecl::group(g)).await;
        let mut samples = Vec::new();
        for vis in ["public", "group"] {
            let sample = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO samples (id, name, sample_type, prepared_by, content_hash, owner_group_id, visibility) \
                 VALUES ($1, 's', 'chemical', $2, decode(md5($1::text) || md5($1::text), 'hex'), $3, $4)",
            )
            .bind(sample)
            .bind(h1.agent)
            .bind(g)
            .bind(vis)
            .execute(a)
            .await
            .unwrap();
            samples.push(sample);
        }
        for (sample, refused) in [(samples[0], true), (samples[1], false)] {
            let (hn, hv) = bearer(h1.agent);
            let resp = srv
                .post(&format!("/api/v1/eln/samples/{sample}/observations"))
                .add_header(hn, hv)
                .json(&json!({"content": secret, "agent_id": h1.agent}))
                .await;
            let links: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM sample_claims WHERE sample_id = $1 AND claim_id = $2",
            )
            .bind(sample)
            .bind(own)
            .fetch_one(a)
            .await
            .unwrap();
            if refused {
                assert_eq!(
                    resp.status_code(),
                    StatusCode::FORBIDDEN,
                    "full={full}: {}",
                    resp.text()
                );
                assert_eq!(
                    resp.json::<serde_json::Value>()["error"],
                    json!(
                        "refused by the tenancy guard: a public sample attaches public claims only"
                    ),
                    "full={full}"
                );
                assert!(!resp.text().contains(&own.to_string()), "full={full}");
                assert_eq!(links, 0, "full={full}: nothing linked");
            } else {
                assert_eq!(
                    resp.status_code(),
                    StatusCode::OK,
                    "full={full}: {}",
                    resp.text()
                );
                assert_eq!(
                    links, 1,
                    "full={full}: the group sample links its group's claim"
                );
            }
        }
    }
}

/// E1d delta D3: the synthesis-membership half of the claim-attach rule.
/// A `group` synthesis of H1's personal group cannot take H1's OTHER
/// group's non-public claim as a member (stage 2 would seed it through H1's
/// full viewer): the whole set is refused with the guard's words and nothing
/// is written, both at 5034 (before the guard exists) and on the full
/// schema; the synthesis group's own non-public claim is admitted. The
/// synthesis is `group`, so no narrowing is involved. Kills: the attach
/// check removed from `SynthesisMembershipRepository::replace_for_synthesis`
/// (at 5034 the foreign claim would become a member).
#[tokio::test]
async fn a_synthesis_never_takes_another_groups_claim_as_a_member() {
    for full in [false, true] {
        let db = if full {
            TestDb::fresh().await
        } else {
            at_5034().await
        };
        let a = &db.admin;
        let h1 = testdb::principal(a, "h1").await;
        let g = h1.personal_group;
        let team = testdb::team_group(a, &h1, &[]).await;
        let theirs = testdb::claim(
            a,
            h1.agent,
            &format!("team finding {}", Uuid::new_v4()),
            0.8,
            TenancyDecl::group(team),
        )
        .await;
        let own = testdb::claim(
            a,
            h1.agent,
            &format!("own finding {}", Uuid::new_v4()),
            0.8,
            TenancyDecl::group(g),
        )
        .await;
        assert_eq!(testdb::claim_pair(a, theirs).await, ("group".into(), team));
        let s = synthesis(a, h1.agent, "group", g, "q").await;
        let members = |a: PgPool| async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM synthesis_claim_membership WHERE synthesis_id = $1",
            )
            .bind(s)
            .fetch_one(&a)
            .await
            .unwrap()
        };

        let mut tx = a.begin().await.unwrap();
        let r = episcience_db::SynthesisMembershipRepository::replace_for_synthesis(
            &mut tx,
            s,
            &[own, theirs],
        )
        .await;
        match r {
            Err(episcience_db::errors::DbError::TenancyRefused(m)) => assert_eq!(
                m, "a group claim attaches only to a row owned by the claim's group",
                "full={full}"
            ),
            other => panic!("full={full}: expected the attach refusal, got {other:?}"),
        }
        drop(tx);
        assert_eq!(members(a.clone()).await, 0, "full={full}: nothing written");

        let mut tx = a.begin().await.unwrap();
        episcience_db::SynthesisMembershipRepository::replace_for_synthesis(&mut tx, s, &[own])
            .await
            .unwrap_or_else(|e| panic!("full={full}: own group claim admitted: {e:?}"));
        tx.commit().await.unwrap();
        assert_eq!(members(a.clone()).await, 1, "full={full}");
    }
}
