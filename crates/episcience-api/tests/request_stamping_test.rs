//! E1g: REST and MCP served on the `episcience_app` application login, every
//! request on a session stamped as its caller (the DSN switch).
//!
//! Every server here is built exactly as the binaries build theirs
//! (`EpiscienceDb::connect` on the application login, every boot refusal);
//! fixtures are written on the admin pool of a fresh template clone.
//!
//! T-A6 (operator-link parity), T-A7 (no writable group), T-R1 (a group
//! synthesis on every read surface), T-R2 (group authority, seamless on row
//! security), T-R4 (row security, not the splice).
#[path = "../../episcience-db/tests/support/mod.rs"]
mod testdb;

#[path = "support/token.rs"]
mod token;

#[path = "support/mcp_http.rs"]
mod mcp_http;

use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION};
use axum::http::StatusCode;
use axum_test::TestServer;
use epigraph_core::TenancyDecl;
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use mcp_http::{bearer_auth, start_mcp, McpClient};
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;
use testdb::{principal, team_group, Principal, TestDb};
use token::{jwt_secret_bytes, mint_test_jwt};
use uuid::Uuid;

fn bearer(agent: Uuid) -> (HeaderName, HeaderValue) {
    (
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", mint_test_jwt(agent))).expect("bearer"),
    )
}

async fn rest(pool: &PgPool) -> (TestServer, Arc<MockProvider>) {
    let mock = Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    let embedder: Arc<dyn EmbeddingService> = mock.clone();
    let state = ElnState {
        db: testdb::app_db_for(pool).await,
        blob_dir: std::env::temp_dir().join(format!("episcience-e1g-{}", Uuid::now_v7())),
        jwt_config: Arc::new(JwtConfig::from_secret(&jwt_secret_bytes())),
        max_upload_bytes: 1024 * 1024,
        embedder,
    };
    let _ = std::fs::create_dir_all(&state.blob_dir);
    (
        TestServer::new(episcience_api::create_router(state)).expect("build TestServer"),
        mock,
    )
}

/// An agent with no group membership at all.
async fn bare_agent(pool: &PgPool) -> Uuid {
    let agent = Uuid::new_v4();
    let mut pk = [0u8; 32];
    pk[..16].copy_from_slice(agent.as_bytes());
    pk[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    sqlx::query(
        "INSERT INTO public.agents (id, public_key, display_name, agent_type, role, state) \
         VALUES ($1, $2, $3, 'human', 'custom', 'active')",
    )
    .bind(agent)
    .bind(&pk[..])
    .bind(format!("fixture-bare-{agent}"))
    .execute(pool)
    .await
    .expect("insert bare agent");
    agent
}

async fn count(pool: &PgPool, sql: &str, id: Uuid) -> i64 {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("count")
}

async fn syntheses_by(pool: &PgPool, author: Uuid) -> i64 {
    count(
        pool,
        "SELECT count(*) FROM syntheses WHERE agent_id = $1",
        author,
    )
    .await
}

/// T-A6 (REST and MCP): a principal with an operator link (a valid token
/// minted before the link) is refused 403 on a read and on a write, writes
/// nothing, and its MCP `tools/call` is refused while discovery still works.
/// An unlinked principal is served. Kills: the parity read dropped from
/// `resolve_principal`, or the REST middleware / MCP `call_tool` not
/// resolving through it.
#[tokio::test]
async fn t_a6_an_operated_principal_is_refused_on_rest_and_mcp() {
    let db = TestDb::fresh().await;
    let a = db.admin.clone();
    let (srv, _) = rest(&a).await;
    let op = principal(&a, "operator").await;
    let agent = principal(&a, "operated").await;
    let free = principal(&a, "free").await;
    sqlx::query(
        "INSERT INTO operator_links (agent_id, operator_id, operator_group_id) VALUES ($1, $2, $3)",
    )
    .bind(agent.agent)
    .bind(op.agent)
    .bind(op.personal_group)
    .execute(&a)
    .await
    .expect("operator link");

    let (n, v) = bearer(agent.agent);
    let read = srv.get("/api/v1/eln/syntheses").add_header(n, v).await;
    assert_eq!(read.status_code(), StatusCode::FORBIDDEN, "{}", read.text());
    assert!(read.text().contains("operated"), "{}", read.text());
    let (n, v) = bearer(agent.agent);
    let write = srv
        .post("/api/v1/eln/syntheses")
        .add_header(n, v)
        .json(&json!({"query": "operated write"}))
        .await;
    assert_eq!(write.status_code(), StatusCode::FORBIDDEN);
    assert_eq!(syntheses_by(&a, agent.agent).await, 0, "nothing written");

    let (n, v) = bearer(free.agent);
    let ok = srv.get("/api/v1/eln/syntheses").add_header(n, v).await;
    assert_eq!(ok.status_code(), StatusCode::OK, "{}", ok.text());

    let blobs = tempfile::TempDir::new().expect("blob dir");
    let addr = start_mcp(
        a.clone(),
        blobs.path().to_path_buf(),
        bearer_auth(&jwt_secret_bytes()),
    )
    .await;
    let mut client = McpClient::new(addr, Some(mint_test_jwt(agent.agent)));
    assert!(client.initialize().await.is_success());
    assert!(client.list_tools().await.result()["tools"]
        .as_array()
        .is_some_and(|t| !t.is_empty()));
    let refused = client
        .call_tool("synthesize", json!({"query": "operated over mcp"}))
        .await
        .error_message();
    assert!(
        refused.contains("Forbidden") && refused.contains("operated"),
        "{refused}"
    );
    assert_eq!(syntheses_by(&a, agent.agent).await, 0, "nothing written");
}

/// Review E1g finding 3, on REST: when the resolve path's operator-link read
/// fails, an UNLINKED principal with a writable group gets 403 (never 500,
/// never served) on a write, and nothing is written; the same write
/// succeeds before the failure (control). The link function's EXECUTE is
/// revoked on this clone only, after the server connected (its boot probe
/// checks the grant). Kills: `resolve_principal` failing open on a link-read
/// error, or `RequestRefusal::Unresolvable` mapped to a non-403 answer.
#[tokio::test]
async fn a_failed_operator_link_read_is_403_on_rest_with_nothing_written() {
    let db = TestDb::fresh().await;
    let a = db.admin.clone();
    let (srv, _) = rest(&a).await;
    let h1 = principal(&a, "h1").await;
    let (n, v) = bearer(h1.agent);
    let ok = srv
        .post("/api/v1/eln/syntheses")
        .add_header(n, v)
        .json(&json!({"query": "before the failure"}))
        .await;
    assert_eq!(ok.status_code(), StatusCode::ACCEPTED, "{}", ok.text());
    assert_eq!(syntheses_by(&a, h1.agent).await, 1);

    sqlx::query(
        "REVOKE EXECUTE ON FUNCTION public.epigraph_operator_of_author(uuid) \
         FROM PUBLIC, epigraph_app",
    )
    .execute(&a)
    .await
    .expect("revoke on the clone");
    let still: bool = sqlx::query_scalar(
        "SELECT has_function_privilege('episcience_app', \
                'public.epigraph_operator_of_author(uuid)', 'EXECUTE')",
    )
    .fetch_one(&a)
    .await
    .expect("read the privilege");
    assert!(!still, "the application login lost EXECUTE");

    let (n, v) = bearer(h1.agent);
    let refused = srv
        .post("/api/v1/eln/syntheses")
        .add_header(n, v)
        .json(&json!({"query": "after the failure"}))
        .await;
    assert_eq!(
        refused.status_code(),
        StatusCode::FORBIDDEN,
        "{}",
        refused.text()
    );
    assert!(
        refused.text().contains("cannot be resolved"),
        "{}",
        refused.text()
    );
    assert_eq!(syntheses_by(&a, h1.agent).await, 1, "nothing more written");
}

/// T-A7 (REST and MCP): a principal that may write NO group gets 403 on every
/// write before anything is written, and reads public rows only. Kills: the
/// writable check removed from `write_as` (the create would reach the
/// database: the default-group lookup would even provision a group for the
/// caller).
#[tokio::test]
async fn t_a7_no_writable_group_is_refused_writes_and_reads_public_only() {
    let db = TestDb::fresh().await;
    let a = db.admin.clone();
    let (srv, _) = rest(&a).await;
    let owner = principal(&a, "owner").await;
    let public = testdb::pending_synthesis(&a, &owner, episcience_core::Visibility::Public).await;
    let private = testdb::pending_synthesis(&a, &owner, episcience_core::Visibility::Group).await;
    let bare = bare_agent(&a).await;

    let (n, v) = bearer(bare);
    let resp = srv
        .post("/api/v1/eln/syntheses")
        .add_header(n, v)
        .json(&json!({"query": "no writable group"}))
        .await;
    assert_eq!(resp.status_code(), StatusCode::FORBIDDEN, "{}", resp.text());
    assert!(
        resp.text().contains("may write no group"),
        "{}",
        resp.text()
    );
    let (n, v) = bearer(bare);
    let resp = srv
        .post("/api/v1/eln/protocols")
        .add_header(n, v)
        .json(&json!({"title": "t", "steps": []}))
        .await;
    assert_eq!(resp.status_code(), StatusCode::FORBIDDEN, "{}", resp.text());
    assert_eq!(syntheses_by(&a, bare).await, 0);
    assert_eq!(
        count(
            &a,
            "SELECT count(*) FROM protocols WHERE authored_by = $1",
            bare
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &a,
            "SELECT count(*) FROM group_memberships WHERE agent_id = $1",
            bare
        )
        .await,
        0,
        "no group was provisioned for the refused caller"
    );

    let (n, v) = bearer(bare);
    let list = srv
        .get("/api/v1/eln/syntheses")
        .add_query_param("limit", 1000)
        .add_header(n, v)
        .await;
    assert_eq!(list.status_code(), StatusCode::OK);
    let ids: Vec<String> = list
        .json::<serde_json::Value>()
        .as_array()
        .expect("array")
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&public.to_string()));
    assert!(!ids.contains(&private.to_string()));

    let blobs = tempfile::TempDir::new().expect("blob dir");
    let addr = start_mcp(
        a.clone(),
        blobs.path().to_path_buf(),
        bearer_auth(&jwt_secret_bytes()),
    )
    .await;
    let mut client = McpClient::new(addr, Some(mint_test_jwt(bare)));
    assert!(client.initialize().await.is_success());
    let refused = client
        .call_tool("synthesize", json!({"query": "no writable group over mcp"}))
        .await
        .error_message();
    assert!(refused.contains("may write no group"), "{refused}");
    assert_eq!(syntheses_by(&a, bare).await, 0);
}

/// A GROUP synthesis owned by `owner`'s personal group with one cluster, one
/// staleness event and an embedding of `query`, all written on the admin
/// pool (the derived rows take the synthesis' pair).
async fn group_synthesis_with_children(
    pool: &PgPool,
    mock: &MockProvider,
    owner: &Principal,
    query: &str,
) -> Uuid {
    let id = testdb::pending_synthesis(pool, owner, episcience_core::Visibility::Group).await;
    sqlx::query("UPDATE syntheses SET query = $2 WHERE id = $1")
        .bind(id)
        .bind(query)
        .execute(pool)
        .await
        .expect("query");
    sqlx::query(
        "INSERT INTO synthesis_clusters (id, synthesis_id, cluster_index, title, summary, \
         member_claim_ids, support_count, contradict_count) \
         VALUES (gen_random_uuid(), $1, 0, 't', 's', ARRAY[$2]::uuid[], 1, 0)",
    )
    .bind(id)
    .bind(testdb::any_public_claim(pool).await)
    .execute(pool)
    .await
    .expect("cluster");
    episcience_db::SynthesisStalenessRepository::record_event(pool, id, "belief_drift", &[], None)
        .await
        .expect("staleness event");
    let embedding = mock.generate(query).await.expect("embed");
    episcience_db::SynthesisEmbeddingsRepository::upsert(
        pool,
        id,
        &embedding,
        "text-embedding-3-small",
        "narrative_head",
    )
    .await
    .expect("embedding");
    id
}

/// T-R1: H1's `group(H1pg)` synthesis is visible to H1 on list, get, search,
/// clusters, snapshot and staleness, and invisible to H2 on every one (404
/// or absent, exactly like a missing id). Kills: any read surface served
/// on an unstamped or privileged session without its splice, or a
/// readability check dropped from one of them.
#[tokio::test]
async fn t_r1_a_group_synthesis_on_every_read_surface() {
    let db = TestDb::fresh().await;
    let a = db.admin.clone();
    let (srv, mock) = rest(&a).await;
    let h1 = principal(&a, "h1").await;
    let h2 = principal(&a, "h2").await;
    let query = format!("t-r1 group synthesis {}", Uuid::new_v4());
    let id = group_synthesis_with_children(&a, &mock, &h1, &query).await;

    for (who, sees) in [(h1.agent, true), (h2.agent, false)] {
        let (n, v) = bearer(who);
        let list = srv
            .get("/api/v1/eln/syntheses")
            .add_query_param("limit", 1000)
            .add_query_param("include_stale", true)
            .add_header(n, v)
            .await;
        assert_eq!(list.status_code(), StatusCode::OK);
        assert_eq!(list.text().contains(&id.to_string()), sees, "list");

        let (n, v) = bearer(who);
        let search = srv
            .post("/api/v1/eln/syntheses/search")
            .add_header(n, v)
            .json(&json!({"query": query, "limit": 50, "include_stale": true}))
            .await;
        assert_eq!(search.status_code(), StatusCode::OK, "{}", search.text());
        assert_eq!(search.text().contains(&id.to_string()), sees, "search");

        for path in ["", "/clusters", "/snapshot", "/staleness"] {
            let (n, v) = bearer(who);
            let r = srv
                .get(&format!("/api/v1/eln/syntheses/{id}{path}"))
                .add_header(n, v)
                .await;
            let want = if sees {
                StatusCode::OK
            } else {
                StatusCode::NOT_FOUND
            };
            assert_eq!(r.status_code(), want, "{path}: {}", r.text());
            if sees && path == "/clusters" {
                assert_eq!(r.json::<serde_json::Value>().as_array().unwrap().len(), 1);
            }
            if sees && path == "/staleness" {
                assert_eq!(r.json::<serde_json::Value>().as_array().unwrap().len(), 1);
            }
        }
    }
}

/// T-R2 (seamless, on row security): H1 creates a `group(T)` synthesis over
/// REST; H2 (writer in T) lists, gets and edits it; R (reader in T) gets it
/// and is refused the edit (403); an outsider gets 404. Kills: group
/// authority replaced by author equality anywhere on the path, or the edit
/// using the READ set (R could edit).
#[tokio::test]
async fn t_r2_team_writers_edit_and_readers_read_a_team_synthesis() {
    let db = TestDb::fresh().await;
    let a = db.admin.clone();
    let (srv, _) = rest(&a).await;
    let h1 = principal(&a, "h1").await;
    let h2 = principal(&a, "h2").await;
    let r = principal(&a, "r").await;
    let outsider = principal(&a, "outsider").await;
    let team = team_group(&a, &h1, &[(h2.agent, "writer"), (r.agent, "reader")]).await;

    let (n, v) = bearer(h1.agent);
    let created = srv
        .post("/api/v1/eln/syntheses")
        .add_header(n, v)
        .json(&json!({"query": "team synthesis", "owner_group_id": team}))
        .await;
    assert_eq!(
        created.status_code(),
        StatusCode::ACCEPTED,
        "{}",
        created.text()
    );
    let id: Uuid = created.json::<serde_json::Value>()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let (owner, author, principal_id): (Uuid, Uuid, Uuid) = sqlx::query_as(
        "SELECT s.owner_group_id, s.agent_id, j.principal_id \
           FROM syntheses s JOIN synthesis_jobs j ON j.id = s.id WHERE s.id = $1",
    )
    .bind(id)
    .fetch_one(&a)
    .await
    .expect("stored");
    assert_eq!(
        (owner, author, principal_id),
        (team, h1.agent, h1.agent),
        "owned by the team; authored by and acting as the caller"
    );

    for (who, get, edit) in [
        (h2.agent, StatusCode::OK, StatusCode::NO_CONTENT),
        (r.agent, StatusCode::OK, StatusCode::FORBIDDEN),
        (outsider.agent, StatusCode::NOT_FOUND, StatusCode::NOT_FOUND),
    ] {
        let (n, v) = bearer(who);
        let g = srv
            .get(&format!("/api/v1/eln/syntheses/{id}"))
            .add_header(n, v)
            .await;
        assert_eq!(g.status_code(), get, "{}", g.text());
        let (n, v) = bearer(who);
        let e = srv
            .patch(&format!("/api/v1/eln/syntheses/{id}/visibility"))
            .add_header(n, v)
            .json(&json!({"visibility": "group"}))
            .await;
        assert_eq!(e.status_code(), edit, "{}", e.text());
    }
    let (n, v) = bearer(h2.agent);
    let del = srv
        .delete(&format!("/api/v1/eln/syntheses/{id}"))
        .add_header(n, v)
        .await;
    assert_eq!(del.status_code(), StatusCode::NO_CONTENT, "{}", del.text());
    let status: String = sqlx::query_scalar("SELECT status FROM syntheses WHERE id = $1")
        .bind(id)
        .fetch_one(&a)
        .await
        .unwrap();
    assert_eq!(status, "deleted", "a team writer's edit landed");
}

/// T-R4 on the application login: ROW SECURITY, not the splice, hides
/// another group's claim and synthesis. The statements below carry no
/// viewer predicate at all and run on the very sessions the handlers get
/// (`read_as`): H2 sees public rows only, H1 sees its group rows too.
/// Kills: `read_as` handing out an unstamped or privileged session (H2
/// would see everything, H1 nothing private).
#[tokio::test]
async fn t_r4_row_security_alone_hides_another_groups_rows() {
    let db = TestDb::fresh().await;
    let a = db.admin.clone();
    let app = testdb::app_db_for(&a).await;
    let h1 = principal(&a, "h1").await;
    let h2 = principal(&a, "h2").await;
    let word = Uuid::new_v4().simple().to_string();
    let public = testdb::claim(
        &a,
        h1.agent,
        &format!("public {word}"),
        0.7,
        TenancyDecl::public(h1.personal_group),
    )
    .await;
    let private = testdb::claim(
        &a,
        h1.agent,
        &format!("private {word}"),
        0.7,
        TenancyDecl::group(h1.personal_group),
    )
    .await;
    assert_eq!(testdb::claim_pair(&a, private).await.0, "group");
    let sp = testdb::pending_synthesis(&a, &h1, episcience_core::Visibility::Public).await;
    let sg = testdb::pending_synthesis(&a, &h1, episcience_core::Visibility::Group).await;

    for (who, want_claims, want_syntheses) in [
        (h1.agent, vec![public, private], vec![sp, sg]),
        (h2.agent, vec![public], vec![sp]),
    ] {
        let viewer = app.resolve_principal(Some(who)).await.expect("resolves");
        let mut conn = app.read_as(&viewer).await.expect("read_as");
        let mut claims: Vec<Uuid> = sqlx::query_scalar(
            "SELECT c.id FROM claims c WHERE c.content LIKE '%' || $1 || '%' ORDER BY c.id",
        )
        .bind(&word)
        .fetch_all(&mut *conn)
        .await
        .expect("unspliced claim read");
        let mut want = want_claims.clone();
        want.sort();
        claims.sort();
        assert_eq!(claims, want, "claims as {who}");
        let mut syn: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM syntheses WHERE id = ANY($1) ORDER BY id")
                .bind(vec![sp, sg])
                .fetch_all(&mut *conn)
                .await
                .expect("unspliced synthesis read");
        let mut want = want_syntheses.clone();
        want.sort();
        syn.sort();
        assert_eq!(syn, want, "syntheses as {who}");
    }
}

/// Brief E1g requirement 3: the body identity fields (`prepared_by`,
/// `agent_id` of an observation, `uploader_id`, a workflow run's
/// `prepared_by`) are OPTIONAL: absent means the caller, and the stored
/// author is the caller; present and different is 403 with nothing written.
/// Kills: a field made required again (absent would be 422), and a route
/// storing the body's value instead of the bound principal.
#[tokio::test]
async fn body_identity_fields_default_to_the_caller_and_refuse_another() {
    use axum_test::multipart::{MultipartForm, Part};
    let db = TestDb::fresh().await;
    let a = db.admin.clone();
    let (srv, _) = rest(&a).await;
    let h1 = principal(&a, "h1").await;
    let h2 = principal(&a, "h2").await;

    let (n, v) = bearer(h1.agent);
    let s = srv
        .post("/api/v1/eln/samples")
        .add_header(n, v)
        .json(&json!({"name": "no preparer named", "sample_type": "chemical"}))
        .await;
    assert_eq!(s.status_code(), StatusCode::OK, "{}", s.text());
    let sample = s.json::<serde_json::Value>();
    assert_eq!(sample["prepared_by"], json!(h1.agent));
    let sample_id: Uuid = sample["id"].as_str().unwrap().parse().unwrap();

    let (n, v) = bearer(h1.agent);
    let refused = srv
        .post("/api/v1/eln/samples")
        .add_header(n, v)
        .json(&json!({"name": "as h2", "sample_type": "chemical", "prepared_by": h2.agent}))
        .await;
    assert_eq!(refused.status_code(), StatusCode::FORBIDDEN);
    assert_eq!(
        count(
            &a,
            "SELECT count(*) FROM samples WHERE prepared_by = $1",
            h2.agent
        )
        .await,
        0
    );

    let (n, v) = bearer(h1.agent);
    let obs = srv
        .post(&format!("/api/v1/eln/samples/{sample_id}/observations"))
        .add_header(n, v)
        .json(&json!({"content": format!("observed {}", Uuid::new_v4())}))
        .await;
    assert_eq!(obs.status_code(), StatusCode::OK, "{}", obs.text());
    let claim: Uuid = obs.json::<serde_json::Value>()["claim_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let author: Uuid = sqlx::query_scalar("SELECT agent_id FROM claims WHERE id = $1")
        .bind(claim)
        .fetch_one(&a)
        .await
        .unwrap();
    assert_eq!(author, h1.agent);
    let (n, v) = bearer(h1.agent);
    let obs = srv
        .post(&format!("/api/v1/eln/samples/{sample_id}/observations"))
        .add_header(n, v)
        .json(&json!({"content": "as h2", "agent_id": h2.agent}))
        .await;
    assert_eq!(obs.status_code(), StatusCode::FORBIDDEN);

    let form = MultipartForm::new().add_part(
        "file",
        Part::bytes(format!("e1g payload {}", Uuid::now_v7()).into_bytes())
            .file_name("e1g.txt")
            .mime_type("text/plain"),
    );
    let (n, v) = bearer(h1.agent);
    let blob = srv
        .post("/api/v1/eln/blobs")
        .add_header(n, v)
        .multipart(form)
        .await;
    assert_eq!(blob.status_code(), StatusCode::OK, "{}", blob.text());
    assert_eq!(
        blob.json::<serde_json::Value>()["uploader_id"],
        json!(h1.agent)
    );

    let (n, v) = bearer(h1.agent);
    let run = srv
        .post("/api/v1/eln/workflow_runs")
        .add_header(n, v)
        .json(&json!({
            "workflow_id": Uuid::new_v4(),
            "canonical_name": "e1g run",
            "started_at": "2026-09-28T00:00:00Z"
        }))
        .await;
    assert_eq!(run.status_code(), StatusCode::CREATED, "{}", run.text());
    let run_sample: Uuid = run.json::<serde_json::Value>()["sample_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let preparer: Uuid = sqlx::query_scalar("SELECT prepared_by FROM samples WHERE id = $1")
        .bind(run_sample)
        .fetch_one(&a)
        .await
        .unwrap();
    assert_eq!(preparer, h1.agent);
}

/// Review E1g finding 5 (the reviewer's RV-4 fixture): H3 writes team T but
/// its personal-group membership is revoked. A write that names NO owner
/// group falls back to the caller's default (personal) group, which it may
/// not write: 403 naming `owner_group_id`, never 500, and nothing is written
/// (no synthesis, no job, no sample). Naming T is served (202). Over MCP the
/// same default-owner `synthesize` is a caller error naming `owner_group_id`,
/// not an internal error. Kills: `default_group` mapping the kernel's
/// revoked-membership refusal to 500, `default_group` returning a group the
/// caller may not write, or choosing another group on the caller's behalf
/// (the no-owner write would then be served).
#[tokio::test]
async fn a_caller_whose_default_group_is_not_writable_gets_403_naming_owner_group_id() {
    let db = TestDb::fresh().await;
    let a = db.admin.clone();
    let (srv, _) = rest(&a).await;
    let h1 = principal(&a, "h1").await;
    let h3 = principal(&a, "h3").await;
    let team = team_group(&a, &h1, &[(h3.agent, "writer")]).await;
    sqlx::query(
        "UPDATE group_memberships SET revoked_at = now() WHERE group_id = $1 AND agent_id = $2",
    )
    .bind(h3.personal_group)
    .bind(h3.agent)
    .execute(&a)
    .await
    .expect("revoke H3's personal membership");

    let (n, v) = bearer(h3.agent);
    let r = srv
        .post("/api/v1/eln/syntheses")
        .add_header(n, v)
        .json(&json!({"query": "no owner named"}))
        .await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN, "{}", r.text());
    assert!(r.text().contains("owner_group_id"), "{}", r.text());
    assert_eq!(syntheses_by(&a, h3.agent).await, 0, "nothing written");
    assert_eq!(
        count(
            &a,
            "SELECT count(*) FROM synthesis_jobs WHERE principal_id = $1",
            h3.agent
        )
        .await,
        0
    );

    let (n, v) = bearer(h3.agent);
    let r = srv
        .post("/api/v1/eln/samples")
        .add_header(n, v)
        .json(&json!({"name": "no owner named", "sample_type": "chemical"}))
        .await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN, "{}", r.text());
    assert!(r.text().contains("owner_group_id"), "{}", r.text());
    assert_eq!(
        count(
            &a,
            "SELECT count(*) FROM samples WHERE prepared_by = $1",
            h3.agent
        )
        .await,
        0
    );

    let (n, v) = bearer(h3.agent);
    let r = srv
        .post("/api/v1/eln/syntheses")
        .add_header(n, v)
        .json(&json!({"query": "team named", "owner_group_id": team}))
        .await;
    assert_eq!(r.status_code(), StatusCode::ACCEPTED, "{}", r.text());
    assert_eq!(syntheses_by(&a, h3.agent).await, 1);

    // The second arm: H4's personal membership is LIVE but only `reader`, so
    // the kernel returns its personal group as the default and the caller
    // may not write it. The answer is the same 403 naming `owner_group_id`
    // (not the row-security refusal the insert would otherwise hit). Kills:
    // `default_group` without its writable-set check.
    let h4 = principal(&a, "h4").await;
    sqlx::query(
        "UPDATE group_memberships SET role = 'reader' WHERE group_id = $1 AND agent_id = $2",
    )
    .bind(h4.personal_group)
    .bind(h4.agent)
    .execute(&a)
    .await
    .expect("demote H4 to reader of its personal group");
    sqlx::query(
        "INSERT INTO public.group_memberships (group_id, agent_id, wrapped_key_share, epoch, role) \
         VALUES ($1, $2, '\\x00'::bytea, 0, 'writer')",
    )
    .bind(team)
    .bind(h4.agent)
    .execute(&a)
    .await
    .expect("H4 writes T");
    let (n, v) = bearer(h4.agent);
    let r = srv
        .post("/api/v1/eln/syntheses")
        .add_header(n, v)
        .json(&json!({"query": "no owner named, reader of its own group"}))
        .await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN, "{}", r.text());
    assert!(r.text().contains("owner_group_id"), "{}", r.text());
    assert_eq!(syntheses_by(&a, h4.agent).await, 0, "nothing written");

    let blobs = tempfile::TempDir::new().expect("blob dir");
    let addr = start_mcp(
        a.clone(),
        blobs.path().to_path_buf(),
        bearer_auth(&jwt_secret_bytes()),
    )
    .await;
    let mut client = McpClient::new(addr, Some(mint_test_jwt(h3.agent)));
    assert!(client.initialize().await.is_success());
    let refused = client
        .call_tool("synthesize", json!({"query": "no owner over mcp"}))
        .await
        .error_message();
    assert!(
        refused.contains("owner_group_id") && !refused.contains("internal"),
        "{refused}"
    );
    assert_eq!(syntheses_by(&a, h3.agent).await, 1, "nothing more written");
}

/// A kernel repository refusal on a request transaction is the caller's
/// answer (403), as EpiScience's own guard refusals are, while any other
/// kernel error stays 500. A REAL refusal is produced: H1's stamped write
/// transaction on the application login tries to create a kernel claim
/// declared in H2's personal group, which the kernel's claims row security
/// refuses (SQLSTATE 42501). Today every request path authorizes the owner
/// group before it reaches a kernel write, so this pins the mapping itself
/// for the next such path. Kills: the 42501 arm dropped from
/// `From<epigraph_db::DbError> for ApiError` (the refusal would be a 500).
#[tokio::test]
async fn a_kernel_row_security_refusal_on_a_request_transaction_is_403() {
    use axum::response::IntoResponse;

    let db = TestDb::fresh().await;
    let a = db.admin.clone();
    let app = testdb::app_db_for(&a).await;
    let h1 = principal(&a, "h1").await;
    let h2 = principal(&a, "h2").await;
    let v1 = app.resolve_principal(Some(h1.agent)).await.expect("H1");
    let mut tx = app.write_as(&v1).await.expect("H1's write transaction");
    let claim = epigraph_core::Claim::new(
        "declared in another principal's group".to_string(),
        epigraph_core::AgentId::from_uuid(h1.agent),
        [7u8; 32],
        epigraph_core::TruthValue::new(0.6).expect("truth"),
    );
    let err = epigraph_db::ClaimRepository::create_conn(
        &mut tx,
        &claim,
        TenancyDecl::public(h2.personal_group),
    )
    .await
    .expect_err("the kernel refuses a claim in a group H1 may not write");
    let code = match &err {
        epigraph_db::DbError::QueryFailed {
            source: sqlx::Error::Database(d),
        } => d.code().map(|c| c.to_string()),
        _ => None,
    };
    assert_eq!(code.as_deref(), Some("42501"), "{err:?}");
    let answer = episcience_api::errors::ApiError::from(err).into_response();
    assert_eq!(answer.status(), StatusCode::FORBIDDEN);

    let other = episcience_api::errors::ApiError::from(epigraph_db::DbError::InvalidData {
        reason: "not a refusal".into(),
    })
    .into_response();
    assert_eq!(other.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
