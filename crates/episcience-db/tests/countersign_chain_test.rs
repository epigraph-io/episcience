//! T-W12: the countersignature chain runs ACROSS signers, and two concurrent
//! countersignatures of one claim serialise (neither forks the chain).
mod support;
use support::{principal, TestDb};

use epigraph_crypto::ContentHasher;
use episcience_core::Ownership;
use episcience_db::{countersign_links, ledger, CountersignRepository};
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
/// in the append's own transaction. Also kills: the repository not storing
/// its row's `signature_hash` (the next writer would fall back to the raw
/// signature, or be refused), and ignoring an older head's signature (the
/// append after it would start a new chain).
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
    for cs in [&first, &second] {
        let stored: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT signature_hash FROM countersignatures WHERE id = $1")
                .bind(cs.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            stored.as_deref(),
            Some(&ContentHasher::hash(&cs.signature)[..]),
            "each row stores the link the next writer chains on"
        );
    }

    // An older head without a stored link (written before 5037, or by an
    // older binary): the next append hashes its signature itself.
    let older = support::any_public_claim(&pool).await;
    sqlx::query(
        "INSERT INTO countersignatures (claim_id, signer_id, signature_meaning, content_hash, \
             signature, countersigned_by, owner_group_id, visibility) \
         VALUES ($1, $2, 'witnessed', decode(repeat('01', 32), 'hex'), decode(repeat('09', 64), 'hex'), \
                 $2, $3, 'public')",
    )
    .bind(older)
    .bind(h1.agent)
    .bind(h1.personal_group)
    .execute(&pool)
    .await
    .unwrap();
    let after_older = sign(&pool, older, &h2, "witnessed", 11).await;
    assert_eq!(
        after_older.prev_signature_hash.as_deref(),
        Some(&ContentHasher::hash(&[9u8; 64])[..]),
        "an append after an older head chains on that head's signature"
    );

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

/// A countersignature written the way an older writer does (on the admin
/// session): `signature_hash` and `prev_signature_hash` exactly as given.
async fn raw(
    pool: &sqlx::PgPool,
    claim: Uuid,
    who: &support::Principal,
    sig_byte: u8,
    hash: Option<Vec<u8>>,
    prev: Option<Vec<u8>>,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO countersignatures (claim_id, signer_id, signature_meaning, content_hash, \
             signature, countersigned_by, owner_group_id, visibility, signature_hash, prev_signature_hash) \
         VALUES ($1, $2, 'witnessed', decode(repeat('01', 32), 'hex'), $3, $2, $4, 'public', $5, $6) \
         RETURNING id",
    )
    .bind(claim)
    .bind(who.agent)
    .bind(vec![sig_byte; 64])
    .bind(who.personal_group)
    .bind(hash)
    .bind(prev)
    .fetch_one(pool)
    .await
    .expect("raw countersignature")
}

/// Finding E1e-D3: the link hashes are checked and filled by the migration
/// owner. A row with no link hash (an older writer) makes `verify` refuse,
/// naming the backfill; `backfill-signature-hashes` fills exactly that row
/// with the hash of its own signature (a second run fills nothing) and
/// `verify` then passes, the repository's own appends included. A stored
/// hash that is not the hash of its signature, and a link to a hash no
/// countersignature of the SAME claim carries (here: another claim's), are
/// each named; the backfill never rewrites a stored hash. Kills: verify
/// skipping the link findings, a missing hash not reported, a wrong hash not
/// recomputed, links checked across claims (or not at all), a backfill that
/// fills nothing, fills the wrong value, or overwrites a stored hash.
#[tokio::test]
async fn verify_refuses_a_chain_that_is_not_whole_and_the_backfill_fills_missing_links() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let h1 = principal(&pool, "h1").await;
    let h2 = principal(&pool, "h2").await;
    let claim = support::any_public_claim(&pool).await;
    let legacy = raw(&pool, claim, &h1, 0x31, None, None).await;
    let next = sign(&pool, claim, &h2, "witnessed", 0x32).await;
    assert_eq!(
        next.prev_signature_hash.as_deref(),
        Some(&ContentHasher::hash(&[0x31u8; 64])[..])
    );

    let mut conn = ledger::connect_with(db.admin_options()).await.unwrap();
    let e = ledger::verify(&mut conn)
        .await
        .expect_err("a row without a link hash")
        .to_string();
    assert!(
        e.contains("links: 1 countersignature(s) carry no link hash"),
        "{e}"
    );
    assert!(
        !e.contains("stores a link hash") && !e.contains("chains on a hash"),
        "{e}"
    );

    assert_eq!(countersign_links::backfill(&mut conn).await.unwrap(), 1);
    let stored: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT signature_hash FROM countersignatures WHERE id = $1")
            .bind(legacy)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        stored.as_deref(),
        Some(&ContentHasher::hash(&[0x31u8; 64])[..])
    );
    assert_eq!(
        countersign_links::backfill(&mut conn).await.unwrap(),
        0,
        "idempotent"
    );
    ledger::verify(&mut conn)
        .await
        .expect("the chain is whole after the backfill");

    let other = support::any_public_claim(&pool).await;
    let wrong_hash = ContentHasher::hash(&[0x34u8; 64]).to_vec();
    let wrong = raw(&pool, other, &h1, 0x33, Some(wrong_hash.clone()), None).await;
    let cross_claim = raw(
        &pool,
        other,
        &h2,
        0x35,
        Some(ContentHasher::hash(&[0x35u8; 64]).to_vec()),
        Some(ContentHasher::hash(&[0x31u8; 64]).to_vec()),
    )
    .await;
    let e = ledger::verify(&mut conn)
        .await
        .expect_err("a wrong hash and a link to another claim")
        .to_string();
    assert!(
        e.contains(&format!(
            "links: countersignature {wrong} stores a link hash that is not the hash of its signature"
        )),
        "{e}"
    );
    assert!(
        e.contains(&format!(
            "links: countersignature {cross_claim} chains on a hash no countersignature of its claim carries"
        )),
        "{e}"
    );
    for fine in [legacy, next.id] {
        assert!(!e.contains(&fine.to_string()), "{fine} is whole: {e}");
    }
    assert!(!e.contains("carry no link hash"), "{e}");
    assert_eq!(countersign_links::backfill(&mut conn).await.unwrap(), 0);
    let kept: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT signature_hash FROM countersignatures WHERE id = $1")
            .bind(wrong)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(kept, Some(wrong_hash), "a stored hash is never rewritten");
}
