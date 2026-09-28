use epigraph_db::Viewer;
use episcience_core::synthesis::{SubgraphSnapshot, Synthesis, SynthesisStatus, Visibility};
use episcience_core::Ownership;
use sqlx::postgres::PgQueryResult;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::errors::DbError;

pub struct SynthesisRepository;

/// `Ok` when exactly `want` rows were affected, `NotFound` when none were,
/// an error otherwise. Every UPDATE/DELETE in this repository goes through
/// it (or is in the idempotent register of `zero_row_writes.rs`), so a write
/// that silently matched nothing is never reported as done.
pub(crate) fn expect_rows(
    res: PgQueryResult,
    want: u64,
    entity: &str,
    id: Uuid,
) -> Result<(), DbError> {
    match res.rows_affected() {
        n if n == want => Ok(()),
        0 => Err(DbError::NotFound {
            entity: entity.into(),
            id: id.to_string(),
        }),
        n => Err(DbError::Constraint(format!(
            "{entity} {id}: expected {want} row(s) affected, got {n}"
        ))),
    }
}

impl SynthesisRepository {
    /// Insert a pending synthesis. ROOT row: the caller declares its pair
    /// (`owner`); the author is `agent_id` (the calling principal).
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
        owner: Ownership,
    ) -> Result<(), DbError> {
        let mut tx = pool.begin().await?;
        Self::create_pending_tx(
            &mut tx,
            id,
            query,
            agent_id,
            parent_synthesis_id,
            prereq_synthesis_ids,
            llm_provider,
            llm_model,
            owner,
            "baseline",
            None,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Transaction-based variant of [`Self::create_pending`].
    ///
    /// Used by `POST /syntheses` and the MCP `synthesize` tool to insert the
    /// synthesis row and its `synthesis_jobs` row in one transaction.
    ///
    /// `skill_name` selects which `SynthesisSkill` the worker resolves at
    /// job-handler time.
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
        owner: Ownership,
        skill_name: &str,
        autonomy_level: Option<&str>,
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
              content_hash, visibility, owner_group_id, skill_name, autonomy_level)
             VALUES ($1, $2, $3, 'pending', $4, '{}'::jsonb, 'signed_louvain',
              $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(id)
        .bind(query)
        .bind(agent_id)
        .bind(parent_synthesis_id)
        .bind(llm_provider)
        .bind(llm_model)
        .bind(prereq)
        .bind(&zero_hash[..])
        .bind(owner.visibility.as_str())
        .bind(owner.owner_group_id)
        .bind(skill_name)
        .bind(autonomy_level)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// The synthesis row, UNFILTERED. For the worker and for a caller that
    /// has already established readability; request handlers use
    /// [`Self::get_readable`].
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

    /// The synthesis if `viewer` can read it (public, or owned by one of the
    /// viewer's groups; the kernel's `Viewer::splice`). An invisible
    /// synthesis is reported exactly like a missing one.
    pub async fn get_readable(
        pool: &PgPool,
        id: Uuid,
        viewer: &Viewer,
    ) -> Result<Synthesis, DbError> {
        let sql = viewer.splice(
            "SELECT s.* FROM syntheses s WHERE s.id = $1 /* {VISIBILITY:s} */",
            2,
        );
        let mut q = sqlx::query(&sql).bind(id);
        if let Some(groups) = viewer.group_bind() {
            q = q.bind(groups);
        }
        let row = q
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| DbError::NotFound {
                entity: "synthesis".into(),
                id: id.to_string(),
            })?;
        row_to_synthesis(&row)
    }

    /// Whether `viewer` can read synthesis `id` (see [`Self::get_readable`]).
    pub async fn readable_by(pool: &PgPool, id: Uuid, viewer: &Viewer) -> Result<bool, DbError> {
        let sql = viewer.splice(
            "SELECT EXISTS (SELECT 1 FROM syntheses s WHERE s.id = $1 /* {VISIBILITY:s} */)",
            2,
        );
        let mut q = sqlx::query_scalar::<_, bool>(&sql).bind(id);
        if let Some(groups) = viewer.group_bind() {
            q = q.bind(groups);
        }
        Ok(q.fetch_one(pool).await?)
    }

    /// Whether `viewer` may EDIT synthesis `id`: it is owned by one of the
    /// viewer's writable groups (role admin or writer).
    pub async fn writable_by(pool: &PgPool, id: Uuid, viewer: &Viewer) -> Result<bool, DbError> {
        let sql = viewer.splice_write(
            "SELECT EXISTS (SELECT 1 FROM syntheses s WHERE s.id = $1 /* {WRITABLE:s} */)",
            2,
        );
        let mut q = sqlx::query_scalar::<_, bool>(&sql).bind(id);
        if let Some(groups) = viewer.writable_bind() {
            q = q.bind(groups);
        }
        Ok(q.fetch_one(pool).await?)
    }

    /// List syntheses readable by `viewer`, newest first. Soft-deleted rows
    /// (status='deleted') are excluded.
    ///
    /// `include_stale = false` (the default for the REST/MCP surface) hides
    /// rows whose `stale_since IS NOT NULL`. `skill_name = Some(..)` filters
    /// to syntheses produced by the named skill.
    pub async fn list_readable_by(
        pool: &PgPool,
        viewer: &Viewer,
        limit: i64,
        offset: i64,
        include_stale: bool,
        skill_name: Option<&str>,
    ) -> Result<Vec<Synthesis>, DbError> {
        let sql = viewer.splice(
            "SELECT s.* FROM syntheses s
              WHERE s.status != 'deleted'
                AND ($3 OR s.stale_since IS NULL)
                AND ($4::text IS NULL OR s.skill_name = $4)
                /* {VISIBILITY:s} */
              ORDER BY s.created_at DESC
              LIMIT $1 OFFSET $2",
            5,
        );
        let mut q = sqlx::query(&sql)
            .bind(limit)
            .bind(offset)
            .bind(include_stale)
            .bind(skill_name);
        if let Some(groups) = viewer.group_bind() {
            q = q.bind(groups);
        }
        let rows = q.fetch_all(pool).await?;
        rows.iter().map(row_to_synthesis).collect()
    }

    /// Set the status of a synthesis `viewer` may edit. `NotFound` when the
    /// row is absent or not owned by one of the viewer's writable groups (the
    /// authorization and the write are one statement).
    pub async fn update_status_as(
        pool: &PgPool,
        id: Uuid,
        status: SynthesisStatus,
        viewer: &Viewer,
    ) -> Result<(), DbError> {
        let sql = viewer.splice_write(
            "UPDATE syntheses s SET status = $2 WHERE s.id = $1 /* {WRITABLE:s} */",
            3,
        );
        let mut q = sqlx::query(&sql).bind(id).bind(status.as_str());
        if let Some(groups) = viewer.writable_bind() {
            q = q.bind(groups);
        }
        expect_rows(q.execute(pool).await?, 1, "synthesis", id)
    }

    /// Set the visibility of a synthesis `viewer` may edit, in one
    /// transaction. Widening to `public` raises the EpiScience widening
    /// interlock (`episcience.allow_widen`, transaction-local) that the
    /// database's widening guard requires, and clears the `private` deferral
    /// on the synthesis' outbox rows so their kernel edges are written by the
    /// next reconcile. `NotFound` when the row is absent or not writable.
    pub async fn set_visibility_as(
        pool: &PgPool,
        id: Uuid,
        visibility: Visibility,
        viewer: &Viewer,
    ) -> Result<(), DbError> {
        let mut tx = pool.begin().await?;
        if visibility == Visibility::Public {
            // The widening guard's rule, checked here too so that it holds
            // before the guard exists (the deploy window at 5034): every
            // member claim, the parent and every prerequisite public. Same
            // words as the guard.
            let publishable: Option<bool> = sqlx::query_scalar(
                "SELECT NOT EXISTS (SELECT 1 FROM synthesis_claim_membership m
                                      LEFT JOIN claims c ON c.id = m.claim_id
                                     WHERE m.synthesis_id = s.id
                                       AND (c.id IS NULL OR c.visibility::text <> 'public'))
                    AND (s.parent_synthesis_id IS NULL
                         OR EXISTS (SELECT 1 FROM syntheses p
                                     WHERE p.id = s.parent_synthesis_id AND p.visibility = 'public'))
                    AND NOT EXISTS (SELECT 1 FROM unnest(coalesce(s.prereq_synthesis_ids, '{}'::uuid[])) x(id)
                                      LEFT JOIN syntheses p ON p.id = x.id
                                     WHERE p.id IS NULL OR p.visibility <> 'public')
                   FROM syntheses s WHERE s.id = $1",
            )
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
            if publishable == Some(false) {
                return Err(DbError::TenancyRefused(format!(
                    "synthesis {id} cannot be public: a member claim, its parent or a prerequisite is not public"
                )));
            }
            sqlx::query("SELECT set_config('episcience.allow_widen', 'yes', true)")
                .execute(&mut *tx)
                .await?;
        }
        let sql = viewer.splice_write(
            "UPDATE syntheses s SET visibility = $2 WHERE s.id = $1 /* {WRITABLE:s} */",
            3,
        );
        let mut q = sqlx::query(&sql).bind(id).bind(visibility.as_str());
        if let Some(groups) = viewer.writable_bind() {
            q = q.bind(groups);
        }
        expect_rows(q.execute(&mut *tx).await?, 1, "synthesis", id)?;
        if visibility == Visibility::Public {
            // Idempotent: 0..n outbox rows carry the deferral.
            sqlx::query(
                "UPDATE synthesis_provo_edges SET deferred_reason = NULL
                  WHERE synthesis_id = $1 AND deferred_reason = 'private'",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Worker: set the status of synthesis `id` (the job's own row).
    pub async fn update_status(
        pool: &PgPool,
        id: Uuid,
        status: SynthesisStatus,
    ) -> Result<(), DbError> {
        let res = sqlx::query("UPDATE syntheses SET status = $2 WHERE id = $1")
            .bind(id)
            .bind(status.as_str())
            .execute(pool)
            .await?;
        expect_rows(res, 1, "synthesis", id)
    }

    pub async fn save_snapshot(
        pool: &PgPool,
        id: Uuid,
        snap: &SubgraphSnapshot,
    ) -> Result<(), DbError> {
        let json = serde_json::to_value(snap).map_err(|e| DbError::Serialization(e.to_string()))?;
        let res = sqlx::query("UPDATE syntheses SET subgraph_snapshot = $2 WHERE id = $1")
            .bind(id)
            .bind(json)
            .execute(pool)
            .await?;
        expect_rows(res, 1, "synthesis", id)
    }

    /// Transaction-based variant of [`Self::save_snapshot`], used by Stage 2
    /// to persist the snapshot and the membership in one transaction.
    pub async fn save_snapshot_tx(
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
        snap: &SubgraphSnapshot,
    ) -> Result<(), DbError> {
        let json = serde_json::to_value(snap).map_err(|e| DbError::Serialization(e.to_string()))?;
        let res = sqlx::query("UPDATE syntheses SET subgraph_snapshot = $2 WHERE id = $1")
            .bind(id)
            .bind(json)
            .execute(&mut **tx)
            .await?;
        expect_rows(res, 1, "synthesis", id)
    }

    pub async fn save_narrative(
        pool: &PgPool,
        id: Uuid,
        narrative: &str,
        content_hash: &[u8; 32],
    ) -> Result<(), DbError> {
        let res = sqlx::query(
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
        expect_rows(res, 1, "synthesis", id)
    }

    /// Mark a synthesis failed unless it already reached a terminal state.
    /// Conditional on purpose (a late failure never overwrites `complete` or
    /// `deleted`), so 0 rows is a legitimate outcome: registered in
    /// `zero_row_writes.rs`.
    pub async fn mark_failed(pool: &PgPool, id: Uuid, reason: &str) -> Result<(), DbError> {
        // NOTE: the table has CHECK ((status='complete') = (completed_at IS
        // NOT NULL)), so a `failed` row keeps `completed_at` NULL.
        sqlx::query(
            "UPDATE syntheses
             SET status = 'failed',
                 failure_reason = $2
             WHERE id = $1
               AND status NOT IN ('complete', 'deleted')",
        )
        .bind(id)
        .bind(reason)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Mark a synthesis stale. Idempotent (`WHERE stale_since IS NULL`): an
    /// already-stale row keeps its first reason, so 0 rows is legitimate
    /// (registered in `zero_row_writes.rs`).
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
        owner_group_id: row
            .try_get::<Option<Uuid>, _>("owner_group_id")
            .ok()
            .flatten(),
        failure_reason: row
            .try_get::<Option<String>, _>("failure_reason")
            .ok()
            .flatten(),
        autonomy_level: row
            .try_get::<Option<String>, _>("autonomy_level")
            .ok()
            .flatten(),
    })
}
