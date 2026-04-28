use episcience_core::synthesis::{
    Synthesis, SynthesisStatus, SubgraphSnapshot, Visibility,
};
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::errors::DbError;

pub struct SynthesisRepository;

impl SynthesisRepository {
    #[allow(clippy::too_many_arguments)]
    pub async fn create_pending(
        pool: &PgPool,
        id: Uuid,
        query: &str,
        agent_id: Uuid,
        parent_synthesis_id: Option<Uuid>,
        prereq_synthesis_ids: &[Uuid],
        llm_provider: &str,
        llm_model: &str,
        visibility: Visibility,
    ) -> Result<(), DbError> {
        let zero_hash = [0u8; 32];
        let prereq: Option<Vec<Uuid>> = if prereq_synthesis_ids.is_empty() {
            None
        } else {
            Some(prereq_synthesis_ids.to_vec())
        };
        sqlx::query(
            "INSERT INTO syntheses
             (id, query, agent_id, status, parent_synthesis_id, subgraph_snapshot,
              clustering_method, llm_provider, llm_model, prereq_synthesis_ids,
              content_hash, visibility)
             VALUES ($1, $2, $3, 'pending', $4, '{}'::jsonb, 'signed_louvain',
              $5, $6, $7, $8, $9)",
        )
        .bind(id)
        .bind(query)
        .bind(agent_id)
        .bind(parent_synthesis_id)
        .bind(llm_provider)
        .bind(llm_model)
        .bind(prereq)
        .bind(&zero_hash[..])
        .bind(visibility.as_str())
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Transaction-based variant of [`create_pending`].
    ///
    /// Used by Phase 3's `POST /syntheses` route to insert the synthesis row
    /// and the corresponding `synthesis_jobs` row in a single transaction —
    /// either both land or neither, so there's no orphaned synthesis row
    /// without a queued job (or vice versa).
    #[allow(clippy::too_many_arguments)]
    pub async fn create_pending_tx(
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
        query: &str,
        agent_id: Uuid,
        parent_synthesis_id: Option<Uuid>,
        prereq_synthesis_ids: &[Uuid],
        llm_provider: &str,
        llm_model: &str,
        visibility: Visibility,
    ) -> Result<(), DbError> {
        let zero_hash = [0u8; 32];
        let prereq: Option<Vec<Uuid>> = if prereq_synthesis_ids.is_empty() {
            None
        } else {
            Some(prereq_synthesis_ids.to_vec())
        };
        sqlx::query(
            "INSERT INTO syntheses
             (id, query, agent_id, status, parent_synthesis_id, subgraph_snapshot,
              clustering_method, llm_provider, llm_model, prereq_synthesis_ids,
              content_hash, visibility)
             VALUES ($1, $2, $3, 'pending', $4, '{}'::jsonb, 'signed_louvain',
              $5, $6, $7, $8, $9)",
        )
        .bind(id)
        .bind(query)
        .bind(agent_id)
        .bind(parent_synthesis_id)
        .bind(llm_provider)
        .bind(llm_model)
        .bind(prereq)
        .bind(&zero_hash[..])
        .bind(visibility.as_str())
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    pub async fn get_by_id(pool: &PgPool, id: Uuid) -> Result<Synthesis, DbError> {
        let row = sqlx::query("SELECT * FROM syntheses WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| DbError::NotFound {
                entity: "synthesis".into(),
                id: id.to_string(),
            })?;
        row_to_synthesis(&row)
    }

    /// List syntheses readable by `agent` ordered by created_at DESC.
    ///
    /// "Readable" mirrors [`readable_by`]: owner, public, or explicit
    /// `synthesis_shares` row with `permission = 'read'`. Soft-deleted rows
    /// (status='deleted') are excluded.
    ///
    /// `include_stale = false` (the default for the REST/MCP surface) hides
    /// rows whose `stale_since IS NOT NULL`. Set `include_stale = true` to
    /// see drifted syntheses (e.g. for a "needs refresh" UI). This mirrors
    /// the same flag on [`SynthesisEmbeddingsRepository::search`].
    pub async fn list_readable_by(
        pool: &PgPool,
        agent: Uuid,
        limit: i64,
        offset: i64,
        include_stale: bool,
    ) -> Result<Vec<Synthesis>, DbError> {
        let rows = sqlx::query(
            "SELECT s.* FROM syntheses s
              LEFT JOIN synthesis_shares sh
                ON sh.synthesis_id = s.id
                AND sh.shared_with_agent_id = $1
                AND sh.permission = 'read'
              WHERE s.status != 'deleted'
                AND ($4 OR s.stale_since IS NULL)
                AND (s.visibility = 'public'
                     OR s.agent_id = $1
                     OR sh.synthesis_id IS NOT NULL)
              ORDER BY s.created_at DESC
              LIMIT $2 OFFSET $3",
        )
        .bind(agent)
        .bind(limit)
        .bind(offset)
        .bind(include_stale)
        .fetch_all(pool)
        .await?;

        rows.iter().map(row_to_synthesis).collect()
    }

    pub async fn readable_by(pool: &PgPool, id: Uuid, agent: Uuid) -> Result<bool, DbError> {
        let row = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
               SELECT 1 FROM syntheses s
                LEFT JOIN synthesis_shares sh
                  ON sh.synthesis_id = s.id AND sh.shared_with_agent_id = $2
               WHERE s.id = $1
                 AND (s.visibility = 'public'
                      OR s.agent_id = $2
                      OR (sh.synthesis_id IS NOT NULL AND sh.permission = 'read'))
             )",
        )
        .bind(id)
        .bind(agent)
        .fetch_one(pool)
        .await?;
        Ok(row)
    }

    pub async fn update_status(
        pool: &PgPool,
        id: Uuid,
        status: SynthesisStatus,
    ) -> Result<(), DbError> {
        sqlx::query("UPDATE syntheses SET status = $2 WHERE id = $1")
            .bind(id)
            .bind(status.as_str())
            .execute(pool)
            .await?;
        Ok(())
    }

    /// Update the visibility column for an existing synthesis row.
    ///
    /// Used by the `PATCH /syntheses/{id}/visibility` route. Owner gating is
    /// enforced at the route layer.
    pub async fn update_visibility(
        pool: &PgPool,
        id: Uuid,
        visibility: Visibility,
    ) -> Result<(), DbError> {
        sqlx::query("UPDATE syntheses SET visibility = $2 WHERE id = $1")
            .bind(id)
            .bind(visibility.as_str())
            .execute(pool)
            .await?;
        Ok(())
    }

    pub async fn save_snapshot(
        pool: &PgPool,
        id: Uuid,
        snap: &SubgraphSnapshot,
    ) -> Result<(), DbError> {
        let json =
            serde_json::to_value(snap).map_err(|e| DbError::Serialization(e.to_string()))?;
        sqlx::query("UPDATE syntheses SET subgraph_snapshot = $2 WHERE id = $1")
            .bind(id)
            .bind(json)
            .execute(pool)
            .await?;
        Ok(())
    }

    /// Transaction-based variant of [`save_snapshot`].
    ///
    /// Used by Stage 2 of the synthesis pipeline to persist snapshot and
    /// membership in a single transaction. The pool-based variant remains for
    /// callers that don't need cross-table atomicity (e.g.
    /// `phase01_e2e_test::test_repos_full_round_trip`).
    pub async fn save_snapshot_tx(
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
        snap: &SubgraphSnapshot,
    ) -> Result<(), DbError> {
        let json =
            serde_json::to_value(snap).map_err(|e| DbError::Serialization(e.to_string()))?;
        sqlx::query("UPDATE syntheses SET subgraph_snapshot = $2 WHERE id = $1")
            .bind(id)
            .bind(json)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    pub async fn save_narrative(
        pool: &PgPool,
        id: Uuid,
        narrative: &str,
        content_hash: &[u8; 32],
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE syntheses
             SET narrative = $2, narrative_format = 'markdown',
                 content_hash = $3, status = 'complete', completed_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(narrative)
        .bind(&content_hash[..])
        .execute(pool)
        .await?;
        Ok(())
    }

    pub async fn mark_failed(pool: &PgPool, id: Uuid, _reason: &str) -> Result<(), DbError> {
        sqlx::query("UPDATE syntheses SET status = 'failed' WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await?;
        Ok(())
    }

    pub async fn mark_stale(pool: &PgPool, id: Uuid, reason: &str) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE syntheses SET stale_since = now(), stale_reason = $2
             WHERE id = $1 AND stale_since IS NULL",
        )
        .bind(id)
        .bind(reason)
        .execute(pool)
        .await?;
        Ok(())
    }
}

fn row_to_synthesis(row: &sqlx::postgres::PgRow) -> Result<Synthesis, DbError> {
    let status = row
        .get::<String, _>("status")
        .parse::<SynthesisStatus>()
        .map_err(|e| DbError::Serialization(format!("invalid status: {e}")))?;
    let visibility = row
        .get::<String, _>("visibility")
        .parse::<Visibility>()
        .map_err(|e| DbError::Serialization(format!("invalid visibility: {e}")))?;

    let snap_json: serde_json::Value = row.get("subgraph_snapshot");
    let subgraph_snapshot: SubgraphSnapshot = serde_json::from_value(snap_json.clone())
        .unwrap_or_else(|_| {
            // If stored as empty object `{}`, reconstruct a minimal valid snapshot
            SubgraphSnapshot {
                claim_ids: vec![],
                edge_ids: vec![],
                belief_intervals: vec![],
                traversal_config: snap_json,
                captured_at: chrono::Utc::now(),
            }
        });

    Ok(Synthesis {
        id: row.get("id"),
        query: row.get("query"),
        agent_id: row.get("agent_id"),
        status,
        parent_synthesis_id: row.get("parent_synthesis_id"),
        narrative: row.get("narrative"),
        narrative_format: row.get("narrative_format"),
        subgraph_snapshot,
        clustering_method: row.get("clustering_method"),
        llm_provider: row.get("llm_provider"),
        llm_model: row.get("llm_model"),
        llm_call_count: row.get("llm_call_count"),
        prereq_synthesis_ids: row.get("prereq_synthesis_ids"),
        created_at: row.get("created_at"),
        completed_at: row.get("completed_at"),
        stale_since: row.get("stale_since"),
        stale_reason: row.get("stale_reason"),
        content_hash: row.get("content_hash"),
        visibility,
    })
}
