//! The countersignature link hashes, checked and filled by the migration
//! owner (a session row security does not filter): `episcience-migrate
//! verify` refuses a database whose chain is not whole, and
//! `episcience-migrate backfill-signature-hashes` fills the hashes older
//! writers left out.
//!
//! Each countersignature stores `signature_hash`, the hash of its own
//! signature, written by the recording session (5037); the next
//! countersignature of the claim chains on it (`prev_signature_hash`,
//! through `episcience_countersign_chain_head`). The database cannot compute
//! the hash (it is not available in SQL), so two things can go wrong:
//!
//! - a row carries NO hash: every row written before 5037, by the E1d
//!   binary between the 5037 apply and the E1e binary install, or during an
//!   `e1e-undo` window. If such a row is a claim's latest countersignature
//!   and a later writer cannot read it, the chain head refuses that writer
//!   (55000) until the next link exists. [`backfill`] fills every missing
//!   hash; [`findings`] names the rows still missing one;
//! - a row carries a WRONG hash, or chains on a hash no row of its claim
//!   carries (a buggy or compromised application writer): [`findings`]
//!   recomputes every hash from the signature and checks every link.
//!
//! The link check is existence only (a `prev_signature_hash` must be the
//! hash of SOME countersignature of the same claim): `created_at` is the
//! writer's transaction start, which does not follow the order in which the
//! per-claim lock let the appends through.
use std::collections::{BTreeMap, BTreeSet};

use epigraph_crypto::ContentHasher;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::ledger::LedgerError;

/// Does this database have the link-hash column (5037 applied)?
async fn has_link_column(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_attribute \
                         WHERE attrelid = 'public.countersignatures'::pg_catalog.regclass \
                           AND attname = 'signature_hash' AND NOT attisdropped)",
    )
    .fetch_one(&mut *conn)
    .await
}

/// Every problem with the stored links, one line each (empty: the chain is
/// whole). Reads every countersignature: run it as the migration owner.
///
/// # Errors
/// A database error.
pub async fn findings(conn: &mut PgConnection) -> Result<Vec<String>, sqlx::Error> {
    if !has_link_column(conn).await? {
        return Ok(vec![
            "links: countersignatures has no signature_hash column (5037 not applied)".into(),
        ]);
    }
    let rows = sqlx::query(
        "SELECT id, claim_id, signature, signature_hash, prev_signature_hash \
           FROM public.countersignatures ORDER BY claim_id, id",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut hashes: BTreeMap<Uuid, BTreeSet<Vec<u8>>> = BTreeMap::new();
    for r in &rows {
        let sig: Vec<u8> = r.get("signature");
        hashes
            .entry(r.get("claim_id"))
            .or_default()
            .insert(ContentHasher::hash(&sig).to_vec());
    }
    let mut out = Vec::new();
    let mut missing = 0usize;
    for r in &rows {
        let id: Uuid = r.get("id");
        let claim: Uuid = r.get("claim_id");
        let sig: Vec<u8> = r.get("signature");
        let stored: Option<Vec<u8>> = r.get("signature_hash");
        let prev: Option<Vec<u8>> = r.get("prev_signature_hash");
        match stored {
            None => missing += 1,
            Some(h) if h.as_slice() != &ContentHasher::hash(&sig)[..] => out.push(format!(
                "links: countersignature {id} stores a link hash that is not the hash of its signature"
            )),
            Some(_) => {}
        }
        if let Some(p) = prev {
            if !hashes.get(&claim).is_some_and(|set| set.contains(&p)) {
                out.push(format!(
                    "links: countersignature {id} chains on a hash no countersignature of its claim carries"
                ));
            }
        }
    }
    if missing > 0 {
        out.insert(
            0,
            format!(
                "links: {missing} countersignature(s) carry no link hash \
                 (run `episcience-migrate backfill-signature-hashes`)"
            ),
        );
    }
    Ok(out)
}

/// Fill `signature_hash` on every countersignature that has none, with the
/// hash of its own signature, in one transaction. Idempotent (a second run
/// fills nothing). Returns the number of rows filled.
///
/// # Errors
/// [`LedgerError::Refused`] before 5037 (no column to fill) or when a row
/// changed under the fill; a database error.
pub async fn backfill(conn: &mut PgConnection) -> Result<u64, LedgerError> {
    if !has_link_column(conn).await? {
        return Err(LedgerError::Refused(
            "backfill-signature-hashes: countersignatures has no signature_hash column \
             (run 5037 first)"
                .into(),
        ));
    }
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let rows = sqlx::query(
        "SELECT id, signature FROM public.countersignatures \
          WHERE signature_hash IS NULL ORDER BY id FOR UPDATE",
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut filled = 0u64;
    for r in &rows {
        let id: Uuid = r.get("id");
        let sig: Vec<u8> = r.get("signature");
        let done = sqlx::query(
            "UPDATE public.countersignatures SET signature_hash = $2 \
              WHERE id = $1 AND signature_hash IS NULL",
        )
        .bind(id)
        .bind(&ContentHasher::hash(&sig)[..])
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if done != 1 {
            return Err(LedgerError::Refused(format!(
                "backfill-signature-hashes: countersignature {id} changed during the fill \
                 ({done} rows updated); nothing was written"
            )));
        }
        filled += 1;
    }
    tx.commit().await?;
    Ok(filled)
}
