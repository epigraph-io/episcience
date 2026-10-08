//! Wiki page registry routes (plan 2026-10-08-wiki-phase-b-articles.md,
//! Task 5): `GET /api/v1/eln/wiki` and `GET /api/v1/eln/wiki/:group_id/:wiki_key`.
//!
//! A page is `(owner_group_id, wiki_key)` over `wiki_article` syntheses; it
//! serves its latest COMPLETE generation, and its history is every generation
//! (any status). Fixtures write `syntheses` rows through the repository on the
//! admin pool (the production write path: `create_pending_tx` with the wiki
//! skill, then `set_wiki_seed_tx`, then `save_narrative` / `mark_failed` /
//! `mark_stale`); the routes read on the caller's stamped session. Every page
//! key is unique to its test (fresh run uuid), so the shared database's other
//! rows never match an assertion. Reads use a READ-ONLY token: the routes need
//! no write scope.
#[path = "../../episcience-db/tests/support/mod.rs"]
mod testdb;

use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION};
use axum::http::StatusCode;
use axum_test::{TestResponse, TestServer};
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use episcience_core::synthesis::Visibility;
use episcience_core::wiki::{article_query, WIKI_SKILL_NAME};
use episcience_core::Ownership;
use episcience_db::SynthesisRepository;
use sqlx::PgPool;
use std::sync::Arc;
use testdb::Principal;
use uuid::Uuid;

#[path = "support/token.rs"]
mod token;
use token::{jwt_secret_bytes, read_only_jwt};

fn bearer(token: &str) -> (HeaderName, HeaderValue) {
    (
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).expect("bearer header"),
    )
}

async fn connect() -> PgPool {
    testdb::shared_pool("DATABASE_URL").await
}

async fn build_test_server(pool: PgPool) -> TestServer {
    let embedder: Arc<dyn EmbeddingService> =
        Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    let state = ElnState {
        db: testdb::app_db_for(&pool).await,
        blob_dir: std::env::temp_dir().join("episcience-wiki-routes-blobs"),
        jwt_config: Arc::new(JwtConfig::from_secret(&jwt_secret_bytes())),
        max_upload_bytes: 1024,
        embedder,
    };
    TestServer::new(episcience_api::create_router(state)).expect("build TestServer")
}

/// A fresh, canonical page key (`WikiKey::as_slug` shape) no other test uses.
fn fresh_key() -> String {
    format!("r{}-c7", Uuid::new_v4().simple())
}

enum Outcome<'a> {
    Complete(&'a str),
    Failed,
}

/// One generation of page `(owner's personal group, key)`, seeded from
/// `seed_theme`, ending in `outcome`.
async fn generation(
    pool: &PgPool,
    owner: &Principal,
    key: &str,
    seed_theme: Uuid,
    query: &str,
    outcome: Outcome<'_>,
) -> Uuid {
    let id = Uuid::now_v7();
    SynthesisRepository::create_pending_tx(
        pool,
        id,
        query,
        owner.agent,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        Ownership::new(owner.personal_group, Visibility::Group),
        WIKI_SKILL_NAME,
        None,
    )
    .await
    .expect("create the wiki synthesis");
    SynthesisRepository::set_wiki_seed_tx(pool, id, seed_theme, key)
        .await
        .expect("set the page key");
    match outcome {
        Outcome::Complete(narrative) => {
            SynthesisRepository::save_narrative(pool, id, narrative, &[7u8; 32])
                .await
                .expect("complete the generation");
        }
        Outcome::Failed => {
            SynthesisRepository::mark_failed(pool, id, "fixture failure")
                .await
                .expect("fail the generation");
        }
    }
    id
}

async fn cleanup(pool: &PgPool, ids: &[Uuid]) {
    for id in ids {
        sqlx::query("DELETE FROM synthesis_jobs WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM syntheses WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await
            .ok();
    }
}

async fn get_as(server: &TestServer, agent: Uuid, path: &str) -> TestResponse {
    let (hn, hv) = bearer(&read_only_jwt(agent));
    server.get(path).add_header(hn, hv).await
}

async fn list_as(server: &TestServer, agent: Uuid) -> Vec<serde_json::Value> {
    let resp = get_as(server, agent, "/api/v1/eln/wiki").await;
    assert_eq!(resp.status_code(), StatusCode::OK, "list: {}", resp.text());
    resp.json()
}

fn page_for<'a>(
    pages: &'a [serde_json::Value],
    group: Uuid,
    key: &str,
) -> Option<&'a serde_json::Value> {
    pages.iter().find(|p| {
        p["owner_group_id"].as_str() == Some(&group.to_string())
            && p["wiki_key"].as_str() == Some(key)
    })
}

fn ids_of(history: &serde_json::Value) -> Vec<String> {
    history
        .as_array()
        .expect("history is an array")
        .iter()
        .map(|v| {
            v["synthesis_id"]
                .as_str()
                .expect("synthesis_id")
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn list_shows_only_pages_of_readable_groups() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;
    let g = testdb::principal(&pool, "wiki-g").await;
    let h = testdb::principal(&pool, "wiki-h").await;
    let (key_g, key_h) = (fresh_key(), fresh_key());
    let label = "Dimensional Lumber, U.S. Sizes";
    let query = article_query(label, "Nominal versus actual sizes.");

    let sg = generation(
        &pool,
        &g,
        &key_g,
        Uuid::new_v4(),
        &query,
        Outcome::Complete("g body"),
    )
    .await;
    let sh = generation(
        &pool,
        &h,
        &key_h,
        Uuid::new_v4(),
        "H topic",
        Outcome::Complete("h body"),
    )
    .await;

    // Positive control: H's page exists and its own group lists it, so its
    // absence from G's list below is visibility, not a broken fixture.
    let h_pages = list_as(&server, h.agent).await;
    assert!(
        page_for(&h_pages, h.personal_group, &key_h).is_some(),
        "H's owner must list H's page: {h_pages:?}"
    );

    let pages = list_as(&server, g.agent).await;
    let mine = page_for(&pages, g.personal_group, &key_g).expect("G's page is listed");
    assert_eq!(mine["synthesis_id"], sg.to_string());
    assert_eq!(
        mine["title"], label,
        "the title is the label line of the query, not the whole query"
    );
    assert!(mine["generated_at"].is_string(), "generated_at: {mine}");
    assert!(
        page_for(&pages, h.personal_group, &key_h).is_none(),
        "G's viewer must not see H's group page"
    );
    assert!(
        pages
            .iter()
            .all(|p| p["owner_group_id"].as_str() != Some(&h.personal_group.to_string())),
        "no row of H's group may reach G's viewer"
    );

    cleanup(&pool, &[sg, sh]).await;
}

#[tokio::test]
async fn latest_complete_wins_over_newer_failed() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;
    let g = testdb::principal(&pool, "wiki-latest").await;
    let key = fresh_key();
    let theme = Uuid::new_v4();

    let v1 = generation(
        &pool,
        &g,
        &key,
        theme,
        "Topic",
        Outcome::Complete("v1 body"),
    )
    .await;
    let v2 = generation(&pool, &g, &key, theme, "Topic", Outcome::Failed).await;

    let pages = list_as(&server, g.agent).await;
    let page = page_for(&pages, g.personal_group, &key).expect("page listed");
    assert_eq!(
        page["synthesis_id"],
        v1.to_string(),
        "the list serves the complete v1"
    );
    assert_eq!(
        pages
            .iter()
            .filter(|p| p["wiki_key"].as_str() == Some(&key))
            .count(),
        1,
        "one page per key"
    );

    let resp = get_as(
        &server,
        g.agent,
        &format!("/api/v1/eln/wiki/{}/{key}", g.personal_group),
    )
    .await;
    assert_eq!(resp.status_code(), StatusCode::OK, "{}", resp.text());
    let d: serde_json::Value = resp.json();
    assert_eq!(d["page"]["synthesis_id"], v1.to_string());
    assert_eq!(d["narrative"], "v1 body", "the served narrative is v1's");
    assert_eq!(
        ids_of(&d["history"]),
        vec![v2.to_string(), v1.to_string()],
        "history: every generation, newest first"
    );
    assert_eq!(d["history"][0]["status"], "failed");
    assert!(d["history"][0]["completed_at"].is_null());
    assert_eq!(d["history"][1]["status"], "complete");

    cleanup(&pool, &[v1, v2]).await;
}

#[tokio::test]
async fn history_spans_reprojected_theme() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;
    let g = testdb::principal(&pool, "wiki-reproj").await;
    let key = fresh_key();
    let (theme_old, theme_new) = (Uuid::new_v4(), Uuid::new_v4());

    let v1 = generation(
        &pool,
        &g,
        &key,
        theme_old,
        "Topic",
        Outcome::Complete("old"),
    )
    .await;
    let v2 = generation(
        &pool,
        &g,
        &key,
        theme_new,
        "Topic",
        Outcome::Complete("new"),
    )
    .await;
    // The two generations really were seeded from different theme rows.
    let seeds: Vec<Uuid> = sqlx::query_scalar(
        "SELECT seed_theme_id FROM syntheses WHERE id = ANY($1) ORDER BY created_at",
    )
    .bind(vec![v1, v2])
    .fetch_all(&pool)
    .await
    .expect("seed themes");
    assert_eq!(seeds, vec![theme_old, theme_new]);

    let pages = list_as(&server, g.agent).await;
    assert_eq!(
        pages
            .iter()
            .filter(|p| p["wiki_key"].as_str() == Some(&key))
            .count(),
        1,
        "a re-projected theme is the same page"
    );
    assert_eq!(
        page_for(&pages, g.personal_group, &key).expect("page")["synthesis_id"],
        v2.to_string(),
        "the newer complete generation is served"
    );

    let resp = get_as(
        &server,
        g.agent,
        &format!("/api/v1/eln/wiki/{}/{key}", g.personal_group),
    )
    .await;
    assert_eq!(resp.status_code(), StatusCode::OK, "{}", resp.text());
    let d: serde_json::Value = resp.json();
    assert_eq!(d["narrative"], "new");
    assert_eq!(ids_of(&d["history"]), vec![v2.to_string(), v1.to_string()]);

    cleanup(&pool, &[v1, v2]).await;
}

#[tokio::test]
async fn get_page_404s_for_other_group_bad_slug_and_no_complete_version() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;
    let g = testdb::principal(&pool, "wiki-404-g").await;
    let h = testdb::principal(&pool, "wiki-404-h").await;
    let (key_g, key_h, key_failed) = (fresh_key(), fresh_key(), fresh_key());

    let sg = generation(
        &pool,
        &g,
        &key_g,
        Uuid::new_v4(),
        "G",
        Outcome::Complete("g"),
    )
    .await;
    let sh = generation(
        &pool,
        &h,
        &key_h,
        Uuid::new_v4(),
        "H",
        Outcome::Complete("h"),
    )
    .await;
    let sf = generation(&pool, &g, &key_failed, Uuid::new_v4(), "F", Outcome::Failed).await;

    // Positive controls: each page serves its own group.
    let own = get_as(
        &server,
        g.agent,
        &format!("/api/v1/eln/wiki/{}/{key_g}", g.personal_group),
    )
    .await;
    assert_eq!(own.status_code(), StatusCode::OK, "{}", own.text());
    let h_own = get_as(
        &server,
        h.agent,
        &format!("/api/v1/eln/wiki/{}/{key_h}", h.personal_group),
    )
    .await;
    assert_eq!(h_own.status_code(), StatusCode::OK, "{}", h_own.text());

    // Another group's page.
    let other = get_as(
        &server,
        g.agent,
        &format!("/api/v1/eln/wiki/{}/{key_h}", h.personal_group),
    )
    .await;
    assert_eq!(
        other.status_code(),
        StatusCode::NOT_FOUND,
        "{}",
        other.text()
    );

    // Malformed slugs: garbage, and a non-canonical spelling of G's own key
    // (`c07` for `c7`), which must not alias the real page.
    let non_canonical = key_g.replace("-c7", "-c07");
    for slug in ["not-a-slug", non_canonical.as_str()] {
        let r = get_as(
            &server,
            g.agent,
            &format!("/api/v1/eln/wiki/{}/{slug}", g.personal_group),
        )
        .await;
        assert_eq!(r.status_code(), StatusCode::NOT_FOUND, "slug {slug}");
    }

    // G's own group, a key with only a failed generation: no page.
    let none = get_as(
        &server,
        g.agent,
        &format!("/api/v1/eln/wiki/{}/{key_failed}", g.personal_group),
    )
    .await;
    assert_eq!(none.status_code(), StatusCode::NOT_FOUND, "{}", none.text());

    // No token: the routes sit behind the bearer gate.
    let anon = server
        .get(&format!("/api/v1/eln/wiki/{}/{key_g}", g.personal_group))
        .await;
    assert_eq!(anon.status_code(), StatusCode::UNAUTHORIZED);
    let anon_list = server.get("/api/v1/eln/wiki").await;
    assert_eq!(anon_list.status_code(), StatusCode::UNAUTHORIZED);

    cleanup(&pool, &[sg, sh, sf]).await;
}

#[tokio::test]
async fn stale_page_reports_stale_since() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;
    let g = testdb::principal(&pool, "wiki-stale").await;
    let (key_stale, key_fresh) = (fresh_key(), fresh_key());

    let stale = generation(
        &pool,
        &g,
        &key_stale,
        Uuid::new_v4(),
        "S",
        Outcome::Complete("s"),
    )
    .await;
    let fresh = generation(
        &pool,
        &g,
        &key_fresh,
        Uuid::new_v4(),
        "F",
        Outcome::Complete("f"),
    )
    .await;
    SynthesisRepository::mark_stale(&pool, stale, "belief_drift")
        .await
        .expect("mark stale");

    let pages = list_as(&server, g.agent).await;
    let s = page_for(&pages, g.personal_group, &key_stale).expect("a stale page stays listed");
    assert!(s["stale_since"].is_string(), "stale page: {s}");
    let f = page_for(&pages, g.personal_group, &key_fresh).expect("fresh page listed");
    assert!(f["stale_since"].is_null(), "fresh page: {f}");

    let resp = get_as(
        &server,
        g.agent,
        &format!("/api/v1/eln/wiki/{}/{key_stale}", g.personal_group),
    )
    .await;
    assert_eq!(resp.status_code(), StatusCode::OK, "{}", resp.text());
    let d: serde_json::Value = resp.json();
    assert_eq!(d["page"]["stale_since"], s["stale_since"]);

    let resp = get_as(
        &server,
        g.agent,
        &format!("/api/v1/eln/wiki/{}/{key_fresh}", g.personal_group),
    )
    .await;
    let d: serde_json::Value = resp.json();
    assert!(d["page"]["stale_since"].is_null());

    cleanup(&pool, &[stale, fresh]).await;
}
