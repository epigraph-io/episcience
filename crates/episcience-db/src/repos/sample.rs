use chrono::Utc;
use epigraph_core::TenancyDecl;
use epigraph_db::Viewer;
use episcience_core::{Ownership, Quantity, Sample, SampleStatus, SampleType, Visibility};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::errors::DbError;

pub struct SampleRepository;

/// The columns every sample read returns (aliased `s`).
const SAMPLE_COLS: &str = "s.id, s.name, s.sample_type, s.status, s.parent_sample_id, \
     s.prepared_by, s.preparation_date, s.expiry_date, s.storage_location, \
     s.quantity_value, s.quantity_unit, s.hazard_info, s.labels, s.properties, \
     s.content_hash, s.created_at, s.updated_at, s.owner_group_id, s.visibility";

impl SampleRepository {
    /// Insert a sample. ROOT row: the caller declares its pair (`owner`); the
    /// author is `prepared_by` (the calling principal). A child of a `group`
    /// sample must be `('group', <the parent's owner>)`; the caller computes
    /// that (see the samples route) and the database refuses anything else.
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        pool: &PgPool,
        name: &str,
        sample_type: SampleType,
        prepared_by: Uuid,
        parent_sample_id: Option<Uuid>,
        storage_location: Option<&str>,
        quantity: Option<&Quantity>,
        hazard_info: &serde_json::Value,
        labels: &[String],
        properties: &serde_json::Value,
        content_hash: &[u8],
        owner: Ownership,
    ) -> Result<Sample, DbError> {
        let id = Uuid::now_v7();
        let now = Utc::now();
        let (q_val, q_unit) = match quantity {
            Some(q) => (Some(q.value), Some(q.unit.as_str())),
            None => (None, None),
        };

        let row = sqlx::query(&format!(
            r#"
            INSERT INTO samples AS s (id, name, sample_type, status, parent_sample_id,
                prepared_by, preparation_date, storage_location,
                quantity_value, quantity_unit, hazard_info, labels, properties,
                content_hash, created_at, updated_at, owner_group_id, visibility)
            VALUES ($1, $2, $3, 'prepared', $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $6, $6, $14, $15)
            RETURNING {SAMPLE_COLS}
            "#
        ))
        .bind(id)
        .bind(name)
        .bind(sample_type.as_str())
        .bind(parent_sample_id)
        .bind(prepared_by)
        .bind(now)
        .bind(storage_location)
        .bind(q_val)
        .bind(q_unit)
        .bind(hazard_info)
        .bind(labels)
        .bind(properties)
        .bind(content_hash)
        .bind(owner.owner_group_id)
        .bind(owner.visibility.as_str())
        .fetch_one(pool)
        .await?;

        row_to_sample(&row)
    }

    /// The sample, UNFILTERED (internal use; request handlers use
    /// [`Self::get_readable`]).
    pub async fn get_by_id(pool: &PgPool, id: Uuid) -> Result<Sample, DbError> {
        let row = sqlx::query(&format!(
            "SELECT {SAMPLE_COLS} FROM samples s WHERE s.id = $1"
        ))
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| DbError::NotFound {
            entity: "sample".into(),
            id: id.to_string(),
        })?;

        row_to_sample(&row)
    }

    /// The sample if `viewer` can read it; an invisible sample is reported
    /// exactly like a missing one.
    pub async fn get_readable(pool: &PgPool, id: Uuid, viewer: &Viewer) -> Result<Sample, DbError> {
        let sql = viewer.splice(
            &format!("SELECT {SAMPLE_COLS} FROM samples s WHERE s.id = $1 /* {{VISIBILITY:s}} */"),
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
                entity: "sample".into(),
                id: id.to_string(),
            })?;
        row_to_sample(&row)
    }

    /// The sample, only when `viewer` may EDIT it (it is owned by one of the
    /// viewer's writable groups).
    ///
    /// A sample the viewer cannot edit is reported exactly like a missing one
    /// (`DbError::NotFound`), so a caller learns nothing it may not act on.
    /// Used by every write that targets an existing sample (status change,
    /// observation, blob attachment, child sample).
    pub async fn get_writable(pool: &PgPool, id: Uuid, viewer: &Viewer) -> Result<Sample, DbError> {
        let sql = viewer.splice_write(
            &format!("SELECT {SAMPLE_COLS} FROM samples s WHERE s.id = $1 /* {{WRITABLE:s}} */"),
            2,
        );
        let mut q = sqlx::query(&sql).bind(id);
        if let Some(groups) = viewer.writable_bind() {
            q = q.bind(groups);
        }
        let row = q
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| DbError::NotFound {
                entity: "sample".into(),
                id: id.to_string(),
            })?;
        row_to_sample(&row)
    }

    /// Samples `viewer` can read, newest first.
    pub async fn list(
        pool: &PgPool,
        viewer: &Viewer,
        status: Option<&str>,
        sample_type: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Sample>, DbError> {
        let sql = viewer.splice(
            &format!(
                "SELECT {SAMPLE_COLS} FROM samples s
                  WHERE ($1::text IS NULL OR s.status = $1)
                    AND ($2::text IS NULL OR s.sample_type = $2)
                    /* {{VISIBILITY:s}} */
                  ORDER BY s.created_at DESC
                  LIMIT $3 OFFSET $4"
            ),
            5,
        );
        let mut q = sqlx::query(&sql)
            .bind(status)
            .bind(sample_type)
            .bind(limit)
            .bind(offset);
        if let Some(groups) = viewer.group_bind() {
            q = q.bind(groups);
        }
        let rows = q.fetch_all(pool).await?;
        rows.iter().map(row_to_sample).collect()
    }

    /// Change the status of a sample `viewer` may edit; `NotFound` when it is
    /// absent or not writable (authorization and write in one statement).
    pub async fn update_status_as(
        pool: &PgPool,
        id: Uuid,
        new_status: SampleStatus,
        viewer: &Viewer,
    ) -> Result<Sample, DbError> {
        let sql = viewer.splice_write(
            &format!(
                "UPDATE samples s SET status = $2, updated_at = NOW()
                  WHERE s.id = $1 /* {{WRITABLE:s}} */
                  RETURNING {SAMPLE_COLS}"
            ),
            3,
        );
        let mut q = sqlx::query(&sql).bind(id).bind(new_status.as_str());
        if let Some(groups) = viewer.writable_bind() {
            q = q.bind(groups);
        }
        let row = q
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| DbError::NotFound {
                entity: "sample".into(),
                id: id.to_string(),
            })?;
        row_to_sample(&row)
    }

    /// Attach an existing claim to a sample. DERIVED row: its pair is the
    /// sample's, set by the database. Idempotent (`ON CONFLICT DO NOTHING`).
    pub async fn link_claim(
        pool: &PgPool,
        sample_id: Uuid,
        claim_id: Uuid,
        relationship: &str,
    ) -> Result<(), DbError> {
        sqlx::query(
            r#"
            INSERT INTO sample_claims (sample_id, claim_id, relationship)
            VALUES ($1, $2, $3)
            ON CONFLICT (sample_id, claim_id) DO NOTHING
            "#,
        )
        .bind(sample_id)
        .bind(claim_id)
        .bind(relationship)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Add an observation claim to a sample: the KERNEL claim through the
    /// kernel's own `ClaimRepository::create_conn` with an explicit tenancy
    /// declaration, and the `sample_claims` link, in one transaction.
    ///
    /// `decl` is the caller's: the sample's own pair when the sample is
    /// `group` (the observation is as private as the sample), otherwise
    /// `public` owned by the author's default group. The kernel deduplicates
    /// by content hash; a duplicate returns the existing claim, which is then
    /// linked.
    ///
    /// Returns the claim id.
    pub async fn add_observation(
        pool: &PgPool,
        sample_id: Uuid,
        agent_id: Uuid,
        content: &str,
        relationship: &str,
        decl: TenancyDecl,
    ) -> Result<Uuid, DbError> {
        let claim = epigraph_core::Claim::new(
            content.to_string(),
            epigraph_core::AgentId::from_uuid(agent_id),
            [0u8; 32],
            epigraph_core::TruthValue::new(0.5)
                .map_err(|e| DbError::Constraint(format!("truth value: {e}")))?,
        );

        let mut tx = pool.begin().await?;
        let stored = epigraph_db::ClaimRepository::create_conn(&mut tx, &claim, decl)
            .await
            .map_err(|e| DbError::Constraint(format!("create observation claim: {e}")))?;
        let claim_id: Uuid = stored.id.into();

        sqlx::query(
            r#"
            INSERT INTO sample_claims (sample_id, claim_id, relationship)
            VALUES ($1, $2, $3)
            ON CONFLICT (sample_id, claim_id) DO NOTHING
            "#,
        )
        .bind(sample_id)
        .bind(claim_id)
        .bind(relationship)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(claim_id)
    }
}

fn row_to_sample(row: &sqlx::postgres::PgRow) -> Result<Sample, DbError> {
    let quantity = match (
        row.get::<Option<f64>, _>("quantity_value"),
        row.get::<Option<String>, _>("quantity_unit"),
    ) {
        (Some(v), Some(u)) => Some(Quantity { value: v, unit: u }),
        _ => None,
    };

    let sample_type = row
        .get::<String, _>("sample_type")
        .parse::<SampleType>()
        .map_err(|e| DbError::Serialization(format!("invalid sample_type: {e}")))?;
    let status = row
        .get::<String, _>("status")
        .parse::<SampleStatus>()
        .map_err(|e| DbError::Serialization(format!("invalid status: {e}")))?;

    Ok(Sample {
        id: row.get("id"),
        name: row.get("name"),
        sample_type,
        status,
        parent_sample_id: row.get("parent_sample_id"),
        prepared_by: row.get("prepared_by"),
        preparation_date: row.get("preparation_date"),
        expiry_date: row.get("expiry_date"),
        storage_location: row.get("storage_location"),
        quantity,
        hazard_info: row.get("hazard_info"),
        labels: row.get("labels"),
        properties: row.get("properties"),
        content_hash: row.get("content_hash"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        owner_group_id: row
            .try_get::<Option<Uuid>, _>("owner_group_id")
            .ok()
            .flatten(),
        visibility: row
            .try_get::<Option<String>, _>("visibility")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<Visibility>().ok()),
    })
}
