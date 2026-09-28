//! T-W12: the countersignature chain runs ACROSS signers, and two concurrent
//! countersignatures of one claim serialise (neither forks the chain).
mod support;
use support::{principal, TestDb};

use epigraph_crypto::ContentHasher;
use episcience_core::Ownership;
use episcience_db::CountersignRepository;
use uuid::Uuid;

async fn sign(
    pool: &sqlx::PgPool,
    claim: Uuid,
    signer: &support::Principal,
    meaning: &str,
    sig_byte: u8,
) -> episcience_core::Countersignature {
    CountersignRepository::create(
        pool,
        claim,
        signer.agent,
        signer.agent,
        meaning,
        &[1u8; 32],
        &[sig_byte; 64],
        2,
        Ownership::public(signer.personal_group),
    )
    .await
    .expect("countersign")
}

/// Kills: a chain head read per signer (the second signer's prev hash would
/// be NULL), or a head read outside the lock / transaction (two concurrent
/// appends would both read NULL and fork the chain): the repository reads the
/// head through `episcience_countersign_chain_head`, which takes the lock,
/// in the append's own transaction.
#[tokio::test]
async fn the_chain_spans_signers_and_concurrent_appends_serialise() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let h1 = principal(&pool, "h1").await;
    let h2 = principal(&pool, "h2").await;
    let claim = support::any_public_claim(&pool).await;

    let first = sign(&pool, claim, &h1, "witnessed", 7).await;
    let second = sign(&pool, claim, &h2, "witnessed", 8).await;
    assert_eq!(first.prev_signature_hash, None);
    assert_eq!(
        second.prev_signature_hash.as_deref(),
        Some(&ContentHasher::hash(&first.signature)[..]),
        "the second signer chains on the first signer's signature"
    );
    assert_eq!(second.countersigned_by, Some(h2.agent));

    for round in 0..5u8 {
        let claim = support::any_public_claim(&pool).await;
        let (a, b) = tokio::join!(
            sign(&pool, claim, &h1, "approved", 10 + round),
            sign(&pool, claim, &h2, "approved", 20 + round),
        );
        let (head, tail) = if a.prev_signature_hash.is_none() {
            (a, b)
        } else {
            (b, a)
        };
        assert_eq!(
            head.prev_signature_hash, None,
            "round {round}: exactly one head"
        );
        assert_eq!(
            tail.prev_signature_hash.as_deref(),
            Some(&ContentHasher::hash(&head.signature)[..]),
            "round {round}: the other append chains on the head (no fork)"
        );
    }
}
