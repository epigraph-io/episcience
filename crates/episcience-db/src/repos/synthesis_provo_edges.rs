use episcience_core::synthesis::ProvenanceEdge;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::errors::DbError;

pub struct SynthesisProvoEdgesRepository;

impl SynthesisProvoEdgesRepository {
    /// Plans (inserts) provenance edges for a synthesis, within a transaction.
    /// Uses ON CONFLICT DO NOTHING so duplicate planning calls are safe.
    pub async fn plan(
        tx: &mut Transaction<'_, Postgres>,
        synthesis_id: Uuid,
        edges: &[ProvenanceEdge],
    ) -> Result<(), DbError> {
        for edge in edges {
            sqlx::query(
                "INSERT INTO synthesis_provo_edges
                 (synthesis_id, predicate, target_kind, target_id)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT DO NOTHING",
            )
            .bind(synthesis_id)
            .bind(&edge.predicate)
            .bind(&edge.target_kind)
            .bind(edge.target_id)
            .execute(&mut **tx)
            .await?;
        }
        Ok(())
    }

    /// Returns edges that have not yet been written (written_at IS NULL).
    pub async fn list_pending(
        pool: &PgPool,
        synthesis_id: Uuid,
    ) -> Result<Vec<ProvenanceEdge>, DbError> {
        let rows = sqlx::query(
            "SELECT predicate, target_kind, target_id
             FROM synthesis_provo_edges
             WHERE synthesis_id = $1 AND written_at IS NULL
             ORDER BY predicate, target_kind, target_id",
        )
        .bind(synthesis_id)
        .fetch_all(pool)
        .await?;

        rows.iter()
            .map(|r| {
                Ok(ProvenanceEdge {
                    predicate: r.get("predicate"),
                    target_kind: r.get("target_kind"),
                    target_id: r.get("target_id"),
                })
            })
            .collect()
    }

    /// Marks an edge as written and records the epigraph edge ID.
    pub async fn mark_written(
        pool: &PgPool,
        synthesis_id: Uuid,
        predicate: &str,
        target_kind: &str,
        target_id: Uuid,
        edge_id: Uuid,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE synthesis_provo_edges
             SET written_at = now(), epigraph_edge_id = $5
             WHERE synthesis_id = $1 AND predicate = $2
               AND target_kind = $3 AND target_id = $4",
        )
        .bind(synthesis_id)
        .bind(predicate)
        .bind(target_kind)
        .bind(target_id)
        .bind(edge_id)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Records a failed write attempt, incrementing attempt_count.
    pub async fn record_failure(
        pool: &PgPool,
        synthesis_id: Uuid,
        predicate: &str,
        target_kind: &str,
        target_id: Uuid,
        err: &str,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE synthesis_provo_edges
             SET attempt_count = attempt_count + 1, last_error = $5
             WHERE synthesis_id = $1 AND predicate = $2
               AND target_kind = $3 AND target_id = $4",
        )
        .bind(synthesis_id)
        .bind(predicate)
        .bind(target_kind)
        .bind(target_id)
        .bind(err)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Returns count of unwritten (pending) edges for a synthesis.
    pub async fn count_pending(pool: &PgPool, synthesis_id: Uuid) -> Result<i64, DbError> {
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM synthesis_provo_edges
             WHERE synthesis_id = $1 AND written_at IS NULL",
        )
        .bind(synthesis_id)
        .fetch_one(pool)
        .await?;
        Ok(count)
    }
}
