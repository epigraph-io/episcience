//! `SynthesisRepository`: the group-owned read and write predicates (E1d) and
//! the worker's row-count-checked writes.
//!
//! Reads splice the kernel viewer (`public` OR owned by one of the viewer's
//! groups); edits need the owner group in the viewer's WRITABLE set (admin or
//! writer). Authorship is never consulted.
mod support;
use episcience_core::synthesis::{SynthesisStatus, Visibility};
use episcience_db::errors::DbError;
use episcience_db::SynthesisRepository;
use support::{pending_synthesis, principal, team_group, viewer_of, TestDb};
use uuid::Uuid;

#[tokio::test]
async fn create_then_get_round_trip_records_the_declared_pair() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let h1 = principal(&pool, "h1").await;
    let id = pending_synthesis(&pool, &h1, Visibility::Group).await;
    let s = SynthesisRepository::get_by_id(&pool, id).await.unwrap();
    assert_eq!(s.id, id);
    assert!(matches!(s.status, SynthesisStatus::Pending));
    assert_eq!(s.agent_id, h1.agent);
    assert_eq!(s.visibility, Visibility::Group);
    assert_eq!(s.owner_group_id, Some(h1.personal_group));
    let raw: String = sqlx::query_scalar("SELECT visibility FROM syntheses WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(raw, "group", "writes emit the kernel vocabulary");
}

#[tokio::test]
async fn get_by_id_not_found() {
    let db = TestDb::fresh().await;
    let result = SynthesisRepository::get_by_id(&db.admin, Uuid::now_v7()).await;
    assert!(matches!(result.unwrap_err(), DbError::NotFound { .. }));
}

/// A group synthesis is readable by the members of its owner group only; a
/// public one by everyone. Kills: the splice dropped (a stranger reads),
/// the public arm dropped, or an authorship arm reintroduced (a stranger who
/// "authored" nothing is still blocked; covered by the team test below).
#[tokio::test]
async fn readable_by_is_group_membership_or_public() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let h1 = principal(&pool, "h1").await;
    let h2 = principal(&pool, "h2").await;
    let private = pending_synthesis(&pool, &h1, Visibility::Group).await;
    let public = pending_synthesis(&pool, &h1, Visibility::Public).await;
    let v1 = viewer_of(&pool, h1.agent).await;
    let v2 = viewer_of(&pool, h2.agent).await;

    assert!(SynthesisRepository::readable_by(&pool, private, &v1)
        .await
        .unwrap());
    assert!(!SynthesisRepository::readable_by(&pool, private, &v2)
        .await
        .unwrap());
    assert!(SynthesisRepository::readable_by(&pool, public, &v2)
        .await
        .unwrap());
    assert!(matches!(
        SynthesisRepository::get_readable(&pool, private, &v2).await,
        Err(DbError::NotFound { .. })
    ));
    let listed: Vec<Uuid> = SynthesisRepository::list_readable_by(&pool, &v2, 100, 0, false, None)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert!(listed.contains(&public) && !listed.contains(&private));
}

/// T-U1 at the repository level (the seamless UX): H2, a WRITER in team T,
/// reads and edits the `group(T)` synthesis H1 created; R, a READER in T,
/// reads it but every edit matches no row; H2 never sees H1's personal
/// synthesis. Kills: the write predicate using the read set (R could edit),
/// an author-equality check (H2 could not edit), or the writable splice
/// dropped.
#[tokio::test]
async fn a_team_writer_edits_and_a_team_reader_only_reads() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let h1 = principal(&pool, "h1").await;
    let h2 = principal(&pool, "h2").await;
    let r = principal(&pool, "reader").await;
    let t = team_group(&pool, &h1, &[(h2.agent, "writer"), (r.agent, "reader")]).await;
    let id = Uuid::now_v7();
    SynthesisRepository::create_pending(
        &pool,
        id,
        "team synthesis",
        h1.agent,
        None,
        &[],
        "anthropic",
        "claude-3-7",
        episcience_core::Ownership::group(t),
    )
    .await
    .unwrap();
    let personal = pending_synthesis(&pool, &h1, Visibility::Group).await;
    let v2 = viewer_of(&pool, h2.agent).await;
    let vr = viewer_of(&pool, r.agent).await;

    assert!(SynthesisRepository::readable_by(&pool, id, &v2)
        .await
        .unwrap());
    assert!(SynthesisRepository::readable_by(&pool, id, &vr)
        .await
        .unwrap());
    assert!(!SynthesisRepository::readable_by(&pool, personal, &v2)
        .await
        .unwrap());

    assert!(SynthesisRepository::writable_by(&pool, id, &v2)
        .await
        .unwrap());
    assert!(!SynthesisRepository::writable_by(&pool, id, &vr)
        .await
        .unwrap());

    // The reader's edit matches nothing and changes nothing.
    assert!(matches!(
        SynthesisRepository::update_status_as(&pool, id, SynthesisStatus::Deleted, &vr).await,
        Err(DbError::NotFound { .. })
    ));
    assert!(matches!(
        SynthesisRepository::set_visibility_as(
            &mut pool.acquire().await.unwrap(),
            id,
            Visibility::Public,
            &vr
        )
        .await,
        Err(DbError::NotFound { .. })
    ));
    let s = SynthesisRepository::get_by_id(&pool, id).await.unwrap();
    assert!(matches!(s.status, SynthesisStatus::Pending));
    assert_eq!(s.visibility, Visibility::Group);

    // The writer's edit lands.
    SynthesisRepository::update_status_as(&pool, id, SynthesisStatus::Deleted, &v2)
        .await
        .expect("a team writer edits the team synthesis");
    let s = SynthesisRepository::get_by_id(&pool, id).await.unwrap();
    assert!(matches!(s.status, SynthesisStatus::Deleted));
}

/// Widening releases the synthesis' deferred outbox rows in the same
/// transaction; narrowing does not touch them. Kills: the clear dropped, or
/// run on a narrowing.
#[tokio::test]
async fn widening_releases_deferred_outbox_rows() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let h1 = principal(&pool, "h1").await;
    let id = pending_synthesis(&pool, &h1, Visibility::Group).await;
    sqlx::query(
        "INSERT INTO synthesis_provo_edges (synthesis_id, predicate, target_kind, target_id, deferred_reason) \
         VALUES ($1, 'ATTRIBUTED_TO', 'agent', $2, 'private')",
    )
    .bind(id)
    .bind(h1.agent)
    .execute(&pool)
    .await
    .unwrap();
    let v1 = viewer_of(&pool, h1.agent).await;
    let deferred = |pool: sqlx::PgPool| async move {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM synthesis_provo_edges WHERE synthesis_id = $1 AND deferred_reason IS NOT NULL",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    SynthesisRepository::set_visibility_as(
        &mut pool.acquire().await.unwrap(),
        id,
        Visibility::Group,
        &v1,
    )
    .await
    .unwrap();
    assert_eq!(deferred(pool.clone()).await, 1);
    SynthesisRepository::set_visibility_as(
        &mut pool.acquire().await.unwrap(),
        id,
        Visibility::Public,
        &v1,
    )
    .await
    .unwrap();
    assert_eq!(deferred(pool.clone()).await, 0);
}

/// The worker's writes report a write that matched nothing (B-M1). Kills:
/// dropping the rows-affected check (the call would return Ok on a missing
/// row).
#[tokio::test]
async fn worker_writes_refuse_a_missing_row() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let missing = Uuid::now_v7();
    assert!(matches!(
        SynthesisRepository::update_status(&pool, missing, SynthesisStatus::Running).await,
        Err(DbError::NotFound { .. })
    ));
    assert!(matches!(
        SynthesisRepository::save_narrative(&pool, missing, "n", &[0u8; 32]).await,
        Err(DbError::NotFound { .. })
    ));
}

#[tokio::test]
async fn save_narrative_marks_complete() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let h1 = principal(&pool, "h1").await;
    let id = pending_synthesis(&pool, &h1, Visibility::Group).await;
    let hash = [7u8; 32];
    SynthesisRepository::save_narrative(&pool, id, "# Title\nbody", &hash)
        .await
        .unwrap();
    let s = SynthesisRepository::get_by_id(&pool, id).await.unwrap();
    assert!(matches!(s.status, SynthesisStatus::Complete));
    assert_eq!(s.narrative.as_deref(), Some("# Title\nbody"));
    assert!(s.completed_at.is_some());
}

#[tokio::test]
async fn mark_failed_sets_status_and_never_clobbers_a_terminal_state() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let h1 = principal(&pool, "h1").await;
    let id = pending_synthesis(&pool, &h1, Visibility::Group).await;
    SynthesisRepository::mark_failed(&pool, id, "timeout")
        .await
        .unwrap();
    let s = SynthesisRepository::get_by_id(&pool, id).await.unwrap();
    assert!(matches!(s.status, SynthesisStatus::Failed));
    assert_eq!(s.failure_reason.as_deref(), Some("timeout"));
    assert!(s.completed_at.is_none());

    let complete_id = pending_synthesis(&pool, &h1, Visibility::Group).await;
    SynthesisRepository::save_narrative(&pool, complete_id, "done", &[0u8; 32])
        .await
        .unwrap();
    SynthesisRepository::mark_failed(&pool, complete_id, "should be ignored")
        .await
        .unwrap();
    let still = SynthesisRepository::get_by_id(&pool, complete_id)
        .await
        .unwrap();
    assert!(matches!(still.status, SynthesisStatus::Complete));
    assert!(still.failure_reason.is_none());
}

#[tokio::test]
async fn mark_stale_sets_stale_since() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let h1 = principal(&pool, "h1").await;
    let id = pending_synthesis(&pool, &h1, Visibility::Group).await;
    SynthesisRepository::mark_stale(&pool, id, "belief_drift")
        .await
        .unwrap();
    let s = SynthesisRepository::get_by_id(&pool, id).await.unwrap();
    assert!(s.stale_since.is_some());
    assert_eq!(s.stale_reason.as_deref(), Some("belief_drift"));
}
