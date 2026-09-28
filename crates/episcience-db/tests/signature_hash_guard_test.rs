//! 5038: a countersignature written by a NON-privileged session must carry
//! its signature hash (the chain link), refused at insert otherwise; a
//! privileged session (the backfill and repair path) is exempt.
//!
//! Brief amendment 2026-09-28: the insert-time refusal lands before the first
//! application login that can write `countersignatures` (the E1f worker is a
//! member of `episcience_rw`).
mod support;
use support::{principal, TestDb, APP_LOGIN};

use epigraph_db::{ScopedPool, ScopedPoolOptions, SessionGucMode};
use uuid::Uuid;

const INSERT: &str = "INSERT INTO countersignatures (id, claim_id, signer_id, signature_meaning, \
     content_hash, signature, signature_version, created_at, countersigned_by, owner_group_id, \
     visibility, signature_hash) \
     VALUES ($1, $2, $3, $7, decode(repeat('01', 32), 'hex'), $4, 2, clock_timestamp(), \
             $3, $5, 'public', $6)";

/// The same row but for its id, signature, meaning (the per-recorder unique
/// key) and hash, on the stamped
/// application login: WITH the hash it is stored, WITHOUT it the guard
/// refuses (23502, naming the hash); the superuser session may store it
/// without. Kills: the trigger dropped, the guard not exempting a privileged
/// session (the backfill path would break), and a guard that refuses a row
/// carrying its hash.
#[tokio::test]
async fn an_application_session_cannot_store_a_countersignature_without_its_link() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = principal(a, "h1").await;
    let claim = support::any_public_claim(a).await;
    let app = ScopedPool::connect_with_options(
        &db.login_url(APP_LOGIN),
        SessionGucMode::Session,
        ScopedPoolOptions::default(),
    )
    .await
    .expect("stamped pool on the app login");
    let (sup, bypass): (bool, bool) =
        sqlx::query_as("SELECT rolsuper, rolbypassrls FROM pg_roles WHERE rolname = session_user")
            .fetch_one(app.inner())
            .await
            .unwrap();
    assert!(!sup && !bypass, "the login under test is unprivileged");
    let viewer = support::viewer_of(a, h1.agent).await;

    let insert = |sig: u8, meaning: &'static str, hash: Option<Vec<u8>>| {
        let app = &app;
        let viewer = &viewer;
        async move {
            let mut tx = app.begin_as(viewer).await.expect("begin_as");
            let r = sqlx::query(INSERT)
                .bind(Uuid::now_v7())
                .bind(claim)
                .bind(h1.agent)
                .bind(vec![sig; 64])
                .bind(h1.personal_group)
                .bind(hash)
                .bind(meaning)
                .execute(&mut *tx)
                .await;
            if r.is_ok() {
                tx.commit().await.expect("commit");
            }
            r
        }
    };

    insert(7, "witnessed", Some(vec![9u8; 32]))
        .await
        .expect("a row carrying its link is stored");
    let e = insert(8, "reviewed", None)
        .await
        .expect_err("a row without its link is refused");
    let d = e.as_database_error().expect("a database error");
    assert_eq!(d.code().as_deref(), Some("23502"), "{e}");
    assert!(d.message().contains("hash of its signature"), "{e}");

    sqlx::query(INSERT)
        .bind(Uuid::now_v7())
        .bind(claim)
        .bind(h1.agent)
        .bind(vec![10u8; 64])
        .bind(h1.personal_group)
        .bind(None::<Vec<u8>>)
        .bind("approved")
        .execute(a)
        .await
        .expect("a privileged session is the repair path and stays exempt");
    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM countersignatures WHERE claim_id = $1")
            .bind(claim)
            .fetch_one(a)
            .await
            .unwrap();
    assert_eq!(stored, 2, "exactly the two admitted rows");
}
