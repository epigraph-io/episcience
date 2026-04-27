use episcience_db::{SynthesisRepository, SynthesisSharesRepository};
use episcience_core::synthesis::{Synthesis, SynthesisStatus, Visibility, SubgraphSnapshot};
use sqlx::PgPool;
use uuid::Uuid;

fn zero_hash() -> [u8; 32] { [0u8; 32] }

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn create_then_get_round_trip(pool: PgPool) {
    let id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    SynthesisRepository::create_pending(
        &pool, id, "test query", owner, None, &[],
        "anthropic", "claude-3-7", Visibility::Private,
    ).await.unwrap();
    let s = SynthesisRepository::get_by_id(&pool, id).await.unwrap();
    assert_eq!(s.id, id);
    assert!(matches!(s.status, SynthesisStatus::Pending));
    assert_eq!(s.query, "test query");
    assert_eq!(s.agent_id, owner);
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn get_by_id_not_found(pool: PgPool) {
    let result = SynthesisRepository::get_by_id(&pool, Uuid::now_v7()).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, episcience_db::errors::DbError::NotFound { .. }));
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn readable_by_predicate_owner_can_read(pool: PgPool) {
    let id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    SynthesisRepository::create_pending(
        &pool, id, "q", owner, None, &[],
        "anthropic", "claude-3-7", Visibility::Private,
    ).await.unwrap();
    assert!(SynthesisRepository::readable_by(&pool, id, owner).await.unwrap());
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn readable_by_predicate_stranger_blocked(pool: PgPool) {
    let id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let stranger = Uuid::now_v7();
    SynthesisRepository::create_pending(
        &pool, id, "q", owner, None, &[],
        "anthropic", "claude-3-7", Visibility::Private,
    ).await.unwrap();
    assert!(!SynthesisRepository::readable_by(&pool, id, stranger).await.unwrap());
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn readable_by_predicate_public_visible_to_all(pool: PgPool) {
    let id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let stranger = Uuid::now_v7();
    SynthesisRepository::create_pending(
        &pool, id, "q", owner, None, &[],
        "anthropic", "claude-3-7", Visibility::Public,
    ).await.unwrap();
    assert!(SynthesisRepository::readable_by(&pool, id, stranger).await.unwrap());
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn update_status_changes_status(pool: PgPool) {
    let id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    SynthesisRepository::create_pending(
        &pool, id, "q", owner, None, &[],
        "anthropic", "claude-3-7", Visibility::Private,
    ).await.unwrap();
    SynthesisRepository::update_status(&pool, id, SynthesisStatus::Running).await.unwrap();
    let s = SynthesisRepository::get_by_id(&pool, id).await.unwrap();
    assert!(matches!(s.status, SynthesisStatus::Running));
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn save_narrative_marks_complete(pool: PgPool) {
    let id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    SynthesisRepository::create_pending(
        &pool, id, "q", owner, None, &[],
        "anthropic", "claude-3-7", Visibility::Private,
    ).await.unwrap();
    let hash = [1u8; 32];
    SynthesisRepository::save_narrative(&pool, id, "narrative text", &hash).await.unwrap();
    let s = SynthesisRepository::get_by_id(&pool, id).await.unwrap();
    assert!(matches!(s.status, SynthesisStatus::Complete));
    assert_eq!(s.narrative.as_deref(), Some("narrative text"));
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn mark_failed_sets_status(pool: PgPool) {
    let id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    SynthesisRepository::create_pending(
        &pool, id, "q", owner, None, &[],
        "anthropic", "claude-3-7", Visibility::Private,
    ).await.unwrap();
    SynthesisRepository::mark_failed(&pool, id, "timeout").await.unwrap();
    let s = SynthesisRepository::get_by_id(&pool, id).await.unwrap();
    assert!(matches!(s.status, SynthesisStatus::Failed));
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn mark_stale_sets_stale_since(pool: PgPool) {
    let id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    SynthesisRepository::create_pending(
        &pool, id, "q", owner, None, &[],
        "anthropic", "claude-3-7", Visibility::Private,
    ).await.unwrap();
    SynthesisRepository::mark_stale(&pool, id, "belief_drift").await.unwrap();
    let s = SynthesisRepository::get_by_id(&pool, id).await.unwrap();
    assert!(s.stale_since.is_some());
    assert_eq!(s.stale_reason.as_deref(), Some("belief_drift"));
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn readable_by_predicate_shared_recipient_can_read(pool: PgPool) {
    let id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let recipient = Uuid::now_v7();
    SynthesisRepository::create_pending(
        &pool, id, "q", owner, None, &[],
        "anthropic", "claude-3-7", Visibility::Shared,
    ).await.unwrap();
    SynthesisSharesRepository::grant(&pool, id, recipient, owner).await.unwrap();
    assert!(SynthesisRepository::readable_by(&pool, id, recipient).await.unwrap());
}
