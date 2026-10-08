//! Integration tests for the Phase 8 ELN-write MCP tools:
//! `propose_protocol`, `add_observation`, `countersign`, `attach_blob`, and
//! the wiki Phase B `wiki_generate_article`.
//!
//! Strategy mirrors `mcp_tools_test.rs`: call the `EpiscienceServer` tool
//! methods directly, then verify the DB side-effect with a raw query. No
//! stdio transport, no MCP runtime — the tool methods are plain async fns.
//!
//! Each test creates its own throwaway agent (with a real Ed25519 key) and a
//! UUID-suffixed sample so concurrent runs don't collide. Best-effort cleanup
//! happens at the end of each test — leaks during failures are tolerable
//! because the test DB is recreated routinely.
//!
//! Run with:
//!   DATABASE_URL=postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
//!     cargo test -p episcience-api --test mcp_write_tools_test
#[path = "../../episcience-db/tests/support/mod.rs"]
mod testdb;

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use epigraph_crypto::{AgentSigner, ContentHasher};
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::mcp::blobs::AttachBlobArgs;
use episcience_api::mcp::countersigns::CountersignArgs;
use episcience_api::mcp::list_countersignatures::ListCountersignaturesArgs;
use episcience_api::mcp::observations::AddObservationArgs;
use episcience_api::mcp::protocols::{ProposeProtocolArgs, ProtocolStepArg};
use episcience_api::mcp::wiki::WikiGenerateArticleArgs;
use episcience_api::mcp::EpiscienceServer;
use episcience_api::middleware::AuthContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Extensions, RawContent};
use sqlx::{PgPool, Row};
use tempfile::TempDir;
use uuid::Uuid;

/// The run's shared clone of the E1 template (scripts/e1-test-db.sh). Refuses
/// port 5432 and any database name not ending in `_test`; no default DSN.
async fn connect() -> PgPool {
    testdb::shared_pool("DATABASE_URL").await
}

/// Build a `(server, signer, agent_id, blob_dir)` quartet. The signer is the
/// ed25519 key whose public component is recorded against `agent_id` in the
/// `agents` table — countersign tests need both halves. The blob dir is a
/// `TempDir` kept alive by the test so the filesystem write in `attach_blob`
/// succeeds.
async fn build_server(pool: PgPool) -> (EpiscienceServer, AgentSigner, Uuid, TempDir) {
    let signer = AgentSigner::generate();
    let pub_key = signer.public_key();
    let agent_id = Uuid::now_v7();

    // Seed the agent — countersignatures.signer_id, blobs.uploader_id, and
    // samples.prepared_by all FK to agents(id).
    sqlx::query(
        r#"
        INSERT INTO agents (id, public_key, display_name, agent_type, role, state)
        VALUES ($1, $2, $3, 'service', 'custom', 'active')
        "#,
    )
    .bind(agent_id)
    .bind(&pub_key[..])
    .bind(format!("mcp-write-test-{}", agent_id))
    .execute(&pool)
    .await
    .expect("seed test agent");
    // Its personal group, exactly as the OAuth mint provisions a principal.
    sqlx::query("SELECT public.epigraph_ensure_personal_group($1)")
        .bind(agent_id)
        .execute(&pool)
        .await
        .expect("provision the agent's personal group");

    let mock = Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    let embedder: Arc<dyn EmbeddingService> = mock;

    let blob_dir = TempDir::new().expect("create temp blob dir");
    let server = EpiscienceServer::new(
        testdb::app_db_for(&pool).await,
        embedder,
        blob_dir.path().to_path_buf(),
        25 * 1024 * 1024,
    );
    (server, signer, agent_id, blob_dir)
}

/// The `Extensions` rmcp hands a tool after `call_tool` authorized `agent`
/// with read + write scope (the production path inserts exactly this).
async fn as_caller(server: &EpiscienceServer, agent: Uuid) -> Extensions {
    let mut ext = Extensions::new();
    server
        .attach_caller(
            &mut ext,
            AuthContext {
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

/// Insert a `samples` row directly. `propose_sample` is not Phase 8 surface,
/// so the e2e + add_observation tests bypass the SampleRepository::create
/// quantity-parsing path with the minimum the FKs need.
async fn seed_sample(pool: &PgPool, prepared_by: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    let name = format!("mcp-test-sample-{id}");
    let hash = ContentHasher::hash(name.as_bytes());
    sqlx::query(
        r#"
        INSERT INTO samples (id, name, sample_type, prepared_by, content_hash,
                             owner_group_id, visibility)
        SELECT $1, $2, 'biological', $3, $4, g.id, 'public'
          FROM groups g
         WHERE g.kind = 'personal' AND g.did_key = 'did:epigraph:personal:' || $3::text
        "#,
    )
    .bind(id)
    .bind(&name)
    .bind(prepared_by)
    .bind(&hash[..])
    .execute(pool)
    .await
    .expect("seed sample");
    id
}

fn body_json(result: &CallToolResult) -> serde_json::Value {
    let content = result
        .content
        .first()
        .expect("tool result has at least one content item");
    match &content.raw {
        RawContent::Text(t) => serde_json::from_str(&t.text).expect("body is JSON"),
        other => panic!("unexpected content variant: {other:?}"),
    }
}

async fn cleanup_agent(pool: &PgPool, agent_id: Uuid) {
    // Order matters for ON DELETE RESTRICT; clean dependents first.
    sqlx::query("DELETE FROM countersignatures WHERE signer_id = $1")
        .bind(agent_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query(
        "DELETE FROM sample_claims WHERE claim_id IN (SELECT id FROM claims WHERE agent_id = $1)",
    )
    .bind(agent_id)
    .execute(pool)
    .await
    .ok();
    sqlx::query("DELETE FROM claims WHERE agent_id = $1")
        .bind(agent_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM blobs WHERE uploader_id = $1")
        .bind(agent_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM samples WHERE prepared_by = $1")
        .bind(agent_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM protocols WHERE authored_by = $1")
        .bind(agent_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM agents WHERE id = $1")
        .bind(agent_id)
        .execute(pool)
        .await
        .ok();
}

// ─── Test 1: propose_protocol inserts a protocol row ────────────────────────

#[tokio::test]
async fn propose_protocol_inserts_row() {
    let pool = connect().await;
    let (server, _signer, agent_id, _blob_dir) = build_server(pool.clone()).await;
    let caller = as_caller(&server, agent_id).await;

    let result = server
        .propose_protocol(
            Parameters(ProposeProtocolArgs {
                title: "DNA origami annealing".to_string(),
                steps: vec![
                    ProtocolStepArg {
                        order: 1,
                        instruction: "Heat to 95C for 5 min".to_string(),
                        duration_minutes: Some(5.0),
                        temperature_c: Some(95.0),
                        notes: None,
                    },
                    ProtocolStepArg {
                        order: 2,
                        instruction: "Cool linearly to 25C over 14 h".to_string(),
                        duration_minutes: Some(840.0),
                        temperature_c: None,
                        notes: Some("Use thermocycler ramp".to_string()),
                    },
                ],
                equipment: vec!["thermocycler".to_string()],
                safety_notes: None,
                supersedes: None,
                labels: vec!["origami".to_string()],
                properties: serde_json::json!({"buffer": "TAE-Mg"}),
            }),
            caller.clone(),
        )
        .await
        .expect("propose_protocol tool call");

    let body = body_json(&result);
    let proto_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(body["version"].as_i64(), Some(1));

    // Verify the row landed under the auth agent (not some client-supplied
    // identity — the tool has no `authored_by` arg, so this proves the
    // auth_agent_id pin is enforced).
    let row = sqlx::query("SELECT authored_by, title, version FROM protocols WHERE id = $1")
        .bind(proto_id)
        .fetch_one(&pool)
        .await
        .expect("fetch inserted protocol");
    let authored_by: Uuid = row.get("authored_by");
    assert_eq!(authored_by, agent_id, "authored_by must be auth_agent_id");
    let title: String = row.get("title");
    assert_eq!(title, "DNA origami annealing");
    let version: i32 = row.get("version");
    assert_eq!(version, 1);

    cleanup_agent(&pool, agent_id).await;
}

// ─── Test 2: add_observation inserts claim + link ───────────────────────────

#[tokio::test]
async fn add_observation_inserts_claim_and_link() {
    let pool = connect().await;
    let (server, _signer, agent_id, _blob_dir) = build_server(pool.clone()).await;
    let caller = as_caller(&server, agent_id).await;
    let sample_id = seed_sample(&pool, agent_id).await;

    let result = server
        .add_observation(
            Parameters(AddObservationArgs {
                sample_id,
                content: "Sample appears as a clear viscous solution.".to_string(),
                relationship: None,
            }),
            caller.clone(),
        )
        .await
        .expect("add_observation tool call");

    let body = body_json(&result);
    let claim_id: Uuid = body["claim_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        body["sample_id"].as_str().unwrap().parse::<Uuid>().unwrap(),
        sample_id
    );
    assert_eq!(body["relationship"].as_str(), Some("observation"));

    // claims row should have agent_id == auth_agent_id (no impersonation).
    let claim_agent: Uuid = sqlx::query_scalar("SELECT agent_id FROM claims WHERE id = $1")
        .bind(claim_id)
        .fetch_one(&pool)
        .await
        .expect("fetch claim");
    assert_eq!(
        claim_agent, agent_id,
        "claim.agent_id must be auth_agent_id"
    );

    // sample_claims link should exist with the expected relationship.
    let link_rel: String = sqlx::query_scalar(
        "SELECT relationship FROM sample_claims WHERE sample_id = $1 AND claim_id = $2",
    )
    .bind(sample_id)
    .bind(claim_id)
    .fetch_one(&pool)
    .await
    .expect("fetch sample_claims link");
    assert_eq!(link_rel, "observation");

    cleanup_agent(&pool, agent_id).await;
}

// ─── Test 3: countersign verifies + inserts a signature row ─────────────────

#[tokio::test]
async fn countersign_verifies_and_inserts() {
    let pool = connect().await;
    let (server, signer, agent_id, _blob_dir) = build_server(pool.clone()).await;
    let caller = as_caller(&server, agent_id).await;
    let sample_id = seed_sample(&pool, agent_id).await;

    // Stage a claim to sign (via the add_observation tool, since that's the
    // canonical write path for sample-linked claims).
    let result = server
        .add_observation(
            Parameters(AddObservationArgs {
                sample_id,
                content: "Initial gel band at 50nm.".to_string(),
                relationship: None,
            }),
            caller.clone(),
        )
        .await
        .expect("add_observation tool call");
    let claim_id: Uuid = body_json(&result)["claim_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    // Build the canonical version-2 message exactly as the route + tool do.
    let signature_meaning = "approved";
    let canonical = format!(
        "{}|{}|{}|{}",
        claim_id, agent_id, signature_meaning, "Initial gel band at 50nm."
    );
    let sig = signer.sign(canonical.as_bytes());
    let public_key_hex = hex::encode(signer.public_key());
    let signature_hex = hex::encode(sig);

    let result = server
        .countersign(
            Parameters(CountersignArgs {
                claim_id,
                signature_meaning: signature_meaning.to_string(),
                signature_hex,
                public_key_hex: Some(public_key_hex),
                signer_id: None,
            }),
            caller.clone(),
        )
        .await
        .expect("countersign tool call");
    let body = body_json(&result);
    let cs_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        body["claim_id"].as_str().unwrap().parse::<Uuid>().unwrap(),
        claim_id
    );
    assert_eq!(
        body["signer_id"].as_str().unwrap().parse::<Uuid>().unwrap(),
        agent_id,
        "signer_id defaults to the authenticated caller"
    );

    // Row should exist with the right signer + version=2.
    let row = sqlx::query(
        "SELECT signer_id, signature_meaning, signature_version FROM countersignatures WHERE id = $1",
    )
    .bind(cs_id)
    .fetch_one(&pool)
    .await
    .expect("fetch countersignature");
    let signer_id: Uuid = row.get("signer_id");
    let meaning: String = row.get("signature_meaning");
    let version: i16 = row.get("signature_version");
    assert_eq!(signer_id, agent_id);
    assert_eq!(meaning, "approved");
    assert_eq!(version, 2);

    cleanup_agent(&pool, agent_id).await;
}

// ─── Test 3b (Phase 8): list_countersignatures returns hex-encoded row ──────

#[tokio::test]
async fn list_countersignatures_returns_signature_row() {
    let pool = connect().await;
    let (server, signer, agent_id, _blob_dir) = build_server(pool.clone()).await;
    let caller = as_caller(&server, agent_id).await;
    let sample_id = seed_sample(&pool, agent_id).await;

    // 1. Stage a claim via add_observation, then countersign it. This
    //    mirrors what the review-bot would later query.
    let obs_content = "list-countersig observation content".to_string();
    let obs_result = server
        .add_observation(
            Parameters(AddObservationArgs {
                sample_id,
                content: obs_content.clone(),
                relationship: None,
            }),
            caller.clone(),
        )
        .await
        .expect("add_observation");
    let claim_id: Uuid = body_json(&obs_result)["claim_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    let signature_meaning = "approved";
    let canonical = format!(
        "{}|{}|{}|{}",
        claim_id, agent_id, signature_meaning, obs_content
    );
    let sig = signer.sign(canonical.as_bytes());
    let public_key_hex = hex::encode(signer.public_key());
    let signature_hex = hex::encode(sig);
    let cs_result = server
        .countersign(
            Parameters(CountersignArgs {
                claim_id,
                signature_meaning: signature_meaning.to_string(),
                signature_hex: signature_hex.clone(),
                public_key_hex: Some(public_key_hex.clone()),
                signer_id: None,
            }),
            caller.clone(),
        )
        .await
        .expect("countersign");
    let cs_id: Uuid = body_json(&cs_result)["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    // 2. List countersignatures via the new MCP tool and assert the row is
    //    present with the expected hex-encoded shape.
    let list_result = server
        .list_countersignatures(
            Parameters(ListCountersignaturesArgs { claim_id }),
            caller.clone(),
        )
        .await
        .expect("list_countersignatures");
    let body = body_json(&list_result);
    let arr = body.as_array().expect("list body is array");
    assert_eq!(arr.len(), 1, "expected exactly one countersignature row");
    let row = &arr[0];
    assert_eq!(
        row["id"].as_str().unwrap().parse::<Uuid>().unwrap(),
        cs_id,
        "row id matches countersign output"
    );
    assert_eq!(
        row["claim_id"].as_str().unwrap().parse::<Uuid>().unwrap(),
        claim_id
    );
    assert_eq!(
        row["signer_id"].as_str().unwrap().parse::<Uuid>().unwrap(),
        agent_id
    );
    assert_eq!(row["signature_meaning"].as_str(), Some("approved"));
    assert_eq!(row["signature_version"].as_i64(), Some(2));
    assert_eq!(
        row["signature_hex"].as_str(),
        Some(signature_hex.as_str()),
        "signature_hex round-trips from countersign input"
    );
    assert_eq!(
        row["public_key_hex"].as_str(),
        Some(public_key_hex.as_str()),
        "public_key_hex matches the seeded agent's key"
    );
    // content_hash_hex is computed by ContentHasher::hash(canonical) =>
    // 64 hex chars (32-byte BLAKE3 digest). We don't recompute the hash
    // here; just sanity-check the shape.
    assert_eq!(
        row["content_hash_hex"].as_str().map(|s| s.len()),
        Some(64),
        "content_hash_hex should be 64 hex chars (32 bytes)"
    );

    cleanup_agent(&pool, agent_id).await;
}

// ─── Test 4: attach_blob decodes base64 + stores file + row ─────────────────

#[tokio::test]
async fn attach_blob_stores_payload_and_row() {
    let pool = connect().await;
    let (server, _signer, agent_id, blob_dir) = build_server(pool.clone()).await;
    let caller = as_caller(&server, agent_id).await;
    let sample_id = seed_sample(&pool, agent_id).await;

    let payload = b"hello phase 8 blob".to_vec();
    let payload_b64 = BASE64_STANDARD.encode(&payload);

    let result = server
        .attach_blob(
            Parameters(AttachBlobArgs {
                file_bytes_base64: payload_b64,
                filename: Some("hello.txt".to_string()),
                mime_type: Some("text/plain".to_string()),
                sample_id: Some(sample_id),
                labels: vec!["test".to_string()],
                properties: serde_json::Value::Null,
            }),
            caller.clone(),
        )
        .await
        .expect("attach_blob tool call");

    let body = body_json(&result);
    let blob_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let content_hash_hex = body["content_hash"].as_str().unwrap().to_string();
    assert_eq!(body["size_bytes"].as_i64(), Some(payload.len() as i64));
    assert_eq!(body["filename"].as_str(), Some("hello.txt"));

    // Verify hash matches BLAKE3 of the payload.
    let expected_hash = ContentHasher::hash(&payload);
    assert_eq!(content_hash_hex, hex::encode(expected_hash));

    // DB row: uploader_id is auth_agent_id (no impersonation).
    let uploader: Uuid = sqlx::query_scalar("SELECT uploader_id FROM blobs WHERE id = $1")
        .bind(blob_id)
        .fetch_one(&pool)
        .await
        .expect("fetch blob");
    assert_eq!(uploader, agent_id, "uploader_id must be auth_agent_id");

    // File on disk: content-addressed path matches.
    let on_disk = blob_dir
        .path()
        .join(&content_hash_hex[0..2])
        .join(&content_hash_hex[2..4])
        .join(format!("{content_hash_hex}.blob"));
    let actual = tokio::fs::read(&on_disk).await.expect("blob file readable");
    assert_eq!(actual, payload, "blob bytes round-trip");

    cleanup_agent(&pool, agent_id).await;
}

// ─── E2E: drive a full ELN turn through MCP only ────────────────────────────
//
// Steps:
//  1. propose_protocol → protocol_id (asserts the row).
//  2. seed_sample directly (propose_sample isn't a Phase 8 tool).
//  3. add_observation → claim_id linked to the sample.
//  4. attach_blob attached to the sample.
//  5. countersign the observation claim.
//
// `synthesize` is exercised by `mcp_tools_test.rs` and would require either
// a running worker or seeding the synth row to `complete` to verify the
// narrative path; we don't repeat it here. The chain in steps 1-5 is what
// "MCP-only write parity" buys.

#[tokio::test]
async fn e2e_eln_turn_through_mcp_only() {
    let pool = connect().await;
    let (server, signer, agent_id, blob_dir) = build_server(pool.clone()).await;
    let caller = as_caller(&server, agent_id).await;

    // 1. propose_protocol
    let proto_result = server
        .propose_protocol(
            Parameters(ProposeProtocolArgs {
                title: "e2e: minimal protocol".to_string(),
                steps: vec![ProtocolStepArg {
                    order: 1,
                    instruction: "do the thing".to_string(),
                    duration_minutes: None,
                    temperature_c: None,
                    notes: None,
                }],
                equipment: vec![],
                safety_notes: None,
                supersedes: None,
                labels: vec![],
                properties: serde_json::Value::Null,
            }),
            caller.clone(),
        )
        .await
        .expect("propose_protocol");
    let proto_id: Uuid = body_json(&proto_result)["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    // 2. seed_sample
    let sample_id = seed_sample(&pool, agent_id).await;

    // 3. add_observation
    let obs_content = "e2e observation content".to_string();
    let obs_result = server
        .add_observation(
            Parameters(AddObservationArgs {
                sample_id,
                content: obs_content.clone(),
                relationship: Some("measurement".to_string()),
            }),
            caller.clone(),
        )
        .await
        .expect("add_observation");
    let claim_id: Uuid = body_json(&obs_result)["claim_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    // 4. attach_blob
    let payload = b"e2e blob payload".to_vec();
    let blob_result = server
        .attach_blob(
            Parameters(AttachBlobArgs {
                file_bytes_base64: BASE64_STANDARD.encode(&payload),
                filename: Some("e2e.bin".to_string()),
                mime_type: Some("application/octet-stream".to_string()),
                sample_id: Some(sample_id),
                labels: vec!["e2e".to_string()],
                properties: serde_json::Value::Null,
            }),
            caller.clone(),
        )
        .await
        .expect("attach_blob");
    let blob_id: Uuid = body_json(&blob_result)["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    // 5. countersign the observation claim
    let signature_meaning = "witnessed";
    let canonical = format!(
        "{}|{}|{}|{}",
        claim_id, agent_id, signature_meaning, obs_content
    );
    let sig = signer.sign(canonical.as_bytes());
    let cs_result = server
        .countersign(
            Parameters(CountersignArgs {
                claim_id,
                signature_meaning: signature_meaning.to_string(),
                signature_hex: hex::encode(sig),
                public_key_hex: Some(hex::encode(signer.public_key())),
                signer_id: None,
            }),
            caller.clone(),
        )
        .await
        .expect("countersign");
    let cs_id: Uuid = body_json(&cs_result)["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    // Verify the full chain landed in the DB.
    let proto_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM protocols WHERE id = $1")
        .bind(proto_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let claim_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM claims WHERE id = $1")
        .bind(claim_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let link_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sample_claims WHERE sample_id = $1 AND claim_id = $2",
    )
    .bind(sample_id)
    .bind(claim_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let blob_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM blobs WHERE id = $1 AND sample_id = $2")
            .bind(blob_id)
            .bind(sample_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let cs_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM countersignatures WHERE id = $1 AND claim_id = $2",
    )
    .bind(cs_id)
    .bind(claim_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(proto_count, 1, "protocol row missing");
    assert_eq!(claim_count, 1, "claim row missing");
    assert_eq!(link_count, 1, "sample_claims link missing");
    assert_eq!(blob_count, 1, "blob row missing (or wrong sample)");
    assert_eq!(cs_count, 1, "countersignature row missing");

    // Keep blob_dir alive until the end so the file write is observable.
    drop(blob_dir);
    cleanup_agent(&pool, agent_id).await;
}

// ─── wiki_generate_article (wiki Phase B, Task 4) ───────────────────────────

/// A theme row (`claim_themes`) with `readable` PUBLIC members (authored by a
/// fresh principal) and `hidden` members that are `group`-private to a team
/// the caller is not in, all current. `properties` is the theme's clustering
/// provenance. Members are kernel claims (`ClaimRepository::create`) moved
/// into the theme on the admin pool, as a clustering run would.
async fn wiki_theme(
    pool: &PgPool,
    description: &str,
    properties: serde_json::Value,
    readable: usize,
    hidden: usize,
) -> Uuid {
    let theme: Uuid = sqlx::query_scalar(
        "INSERT INTO public.claim_themes (label, description, properties) \
         VALUES ('Origami folding', $1, $2) RETURNING id",
    )
    .bind(description)
    .bind(&properties)
    .fetch_one(pool)
    .await
    .expect("insert theme");
    let author = testdb::principal(pool, "wiki-member-author").await;
    let outsider = testdb::principal(pool, "wiki-member-outsider").await;
    let outsiders = testdb::team_group(pool, &outsider, &[]).await;
    let public = epigraph_core::TenancyDecl::public(author.personal_group);
    let private = epigraph_core::TenancyDecl::group(outsiders);
    let decls = (0..readable)
        .map(|_| (author.agent, public))
        .chain((0..hidden).map(|_| (outsider.agent, private)));
    for (i, (who, decl)) in decls.enumerate() {
        let c = testdb::claim(pool, who, &format!("crease fact {i} of {theme}"), 0.9, decl).await;
        sqlx::query("UPDATE public.claims SET theme_id = $2 WHERE id = $1")
            .bind(c)
            .bind(theme)
            .execute(pool)
            .await
            .expect("put the claim in the theme");
    }
    if hidden > 0 {
        let (vis, _): (String, Uuid) = sqlx::query_as(
            "SELECT visibility::text, owner_group_id FROM public.claims \
              WHERE theme_id = $1 AND owner_group_id = $2 LIMIT 1",
        )
        .bind(theme)
        .bind(outsiders)
        .fetch_one(pool)
        .await
        .expect("a hidden member");
        assert_eq!(vis, "group", "hidden members must really be private");
    }
    theme
}

fn cluster_props(run: Uuid) -> serde_json::Value {
    serde_json::json!({"source": "cluster_run", "cluster_run_id": run, "cluster_id": 7})
}

/// `(syntheses, synthesis_jobs)` rows the caller has written.
async fn written_by(pool: &PgPool, agent: Uuid) -> (i64, i64) {
    sqlx::query_as(
        "SELECT (SELECT count(*) FROM syntheses WHERE agent_id = $1), \
                (SELECT count(*) FROM synthesis_jobs WHERE principal_id = $1)",
    )
    .bind(agent)
    .fetch_one(pool)
    .await
    .expect("count the caller's rows")
}

fn wiki_args(theme_id: Uuid, owner_group_id: Option<Uuid>) -> WikiGenerateArticleArgs {
    WikiGenerateArticleArgs {
        theme_id,
        owner_group_id,
        wait_for_completion: false,
        timeout_seconds: 0,
    }
}

/// A caller who is a WRITER of team group G asks for the article of a theme
/// with exactly `MIN_READABLE_MEMBERS` readable members (the boundary is
/// admitted). One `wiki_article` synthesis owned by G (`visibility='group'`)
/// is written with the theme as its seed and the page key derived from the
/// theme's clustering provenance, plus its queued job, whose payload carries
/// the same query and the theme seed. Kills: the article landing in the
/// caller's personal group (owner_group_id ignored), a non-wiki skill, a
/// missing or UUID-derived page key, a job without `seed_theme_id` (the
/// worker would fall back to the off-theme text seed), and an off-by-one
/// that refuses exactly 20.
#[tokio::test]
async fn wiki_generate_creates_a_group_wiki_synthesis() {
    let pool = connect().await;
    let (server, _signer, agent_id, _blob_dir) = build_server(pool.clone()).await;
    let team_admin = testdb::principal(&pool, "wiki-team-admin").await;
    let g = testdb::team_group(&pool, &team_admin, &[(agent_id, "writer")]).await;
    let run = Uuid::now_v7();
    let n = usize::try_from(episcience_core::wiki::MIN_READABLE_MEMBERS).unwrap();
    let theme = wiki_theme(&pool, "How paper folds.", cluster_props(run), n, 0).await;
    let caller = as_caller(&server, agent_id).await;

    let result = server
        .wiki_generate_article(Parameters(wiki_args(theme, Some(g))), caller)
        .await
        .expect("wiki_generate_article");
    let body = body_json(&result);
    let id: Uuid = body["synthesis_id"].as_str().unwrap().parse().unwrap();
    let want_key = format!("r{}-c7", run.simple());
    assert_eq!(body["wiki_key"].as_str(), Some(want_key.as_str()));
    assert_eq!(body["status"].as_str(), Some("queued"));

    let row: (String, String, Uuid, Option<Uuid>, Option<String>, String, Uuid) = sqlx::query_as(
        "SELECT skill_name, visibility::text, owner_group_id, seed_theme_id, wiki_key, query, agent_id \
           FROM syntheses WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .expect("the synthesis row");
    let query = episcience_core::wiki::article_query("Origami folding", "How paper folds.");
    assert_eq!(query, "Origami folding\n\nHow paper folds.");
    assert_eq!(
        row,
        (
            "wiki_article".to_string(),
            "group".to_string(),
            g,
            Some(theme),
            Some(want_key),
            query.clone(),
            agent_id
        )
    );

    let (state, principal, payload): (String, Uuid, serde_json::Value) =
        sqlx::query_as("SELECT state, principal_id, payload FROM synthesis_jobs WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("the job row");
    assert_eq!((state.as_str(), principal), ("queued", agent_id));
    assert_eq!(
        payload["seed_theme_id"].as_str(),
        Some(theme.to_string().as_str())
    );
    assert_eq!(payload["query"].as_str(), Some(query.as_str()));
    assert_eq!(
        payload["synthesis_id"].as_str(),
        Some(id.to_string().as_str())
    );

    for sql in [
        "DELETE FROM synthesis_jobs WHERE id = $1",
        "DELETE FROM syntheses WHERE id = $1",
    ] {
        sqlx::query(sql).bind(id).execute(&pool).await.ok();
    }
}

/// A theme with 25 members of which only 19 are readable by the caller (6
/// are private to a team it is not in) is refused, naming the READABLE
/// count, and nothing is written. Kills: counting every member (25 >= 20
/// would pass), the stored `claim_count`, and a refusal after the synthesis
/// row was inserted. The readable count is enforced twice, by the kernel
/// `/* {VISIBILITY:c} */` splice and by the stamped connection's row
/// security, so dropping only the splice still passes (measured): this pins
/// the outcome, not the splice.
#[tokio::test]
async fn wiki_generate_refuses_thin_theme() {
    let pool = connect().await;
    let (server, _signer, agent_id, _blob_dir) = build_server(pool.clone()).await;
    let theme = wiki_theme(&pool, "", cluster_props(Uuid::now_v7()), 19, 6).await;
    let caller = as_caller(&server, agent_id).await;

    let err = server
        .wiki_generate_article(Parameters(wiki_args(theme, None)), caller)
        .await
        .expect_err("a thin theme is refused");
    assert!(
        err.message.contains("19 readable members"),
        "{}",
        err.message
    );
    assert!(err.message.contains("at least 20"), "{}", err.message);
    assert_eq!(written_by(&pool, agent_id).await, (0, 0));
}

/// A theme without clustering provenance (`properties = {}`) has no stable
/// page key, so it is refused and nothing is written. Kills: falling back to
/// the theme UUID as the key (re-projection would split the page history).
#[tokio::test]
async fn wiki_generate_refuses_theme_without_cluster_properties() {
    let pool = connect().await;
    let (server, _signer, agent_id, _blob_dir) = build_server(pool.clone()).await;
    let theme = wiki_theme(&pool, "", serde_json::json!({}), 20, 0).await;
    let caller = as_caller(&server, agent_id).await;

    let err = server
        .wiki_generate_article(Parameters(wiki_args(theme, None)), caller)
        .await
        .expect_err("no provenance, no page");
    assert!(
        err.message.contains("cluster provenance"),
        "{}",
        err.message
    );
    assert_eq!(written_by(&pool, agent_id).await, (0, 0));
}

/// An id that names no theme is `theme … not found`; nothing is written.
#[tokio::test]
async fn wiki_generate_refuses_unknown_theme() {
    let pool = connect().await;
    let (server, _signer, agent_id, _blob_dir) = build_server(pool.clone()).await;
    let caller = as_caller(&server, agent_id).await;
    let missing = Uuid::new_v4();

    let err = server
        .wiki_generate_article(Parameters(wiki_args(missing, None)), caller)
        .await
        .expect_err("no such theme");
    assert!(
        err.message.contains(&format!("theme {missing} not found")),
        "{}",
        err.message
    );
    assert_eq!(written_by(&pool, agent_id).await, (0, 0));
}

/// A caller who is only a READER of team group H cannot put an article in
/// H, even for a theme that otherwise qualifies: refused by the ownership
/// rule (`root_ownership`), nothing written. Kills: accepting any group the
/// caller can read, or any `owner_group_id` at all.
#[tokio::test]
async fn wiki_generate_refuses_a_group_the_caller_cannot_write() {
    let pool = connect().await;
    let (server, _signer, agent_id, _blob_dir) = build_server(pool.clone()).await;
    let team_admin = testdb::principal(&pool, "wiki-other-admin").await;
    let h = testdb::team_group(&pool, &team_admin, &[(agent_id, "reader")]).await;
    let theme = wiki_theme(&pool, "", cluster_props(Uuid::now_v7()), 20, 0).await;
    let caller = as_caller(&server, agent_id).await;

    let err = server
        .wiki_generate_article(Parameters(wiki_args(theme, Some(h))), caller)
        .await
        .expect_err("a read-only group is refused");
    assert!(
        err.message.contains("not a group the caller may write"),
        "{}",
        err.message
    );
    assert_eq!(written_by(&pool, agent_id).await, (0, 0));
}
