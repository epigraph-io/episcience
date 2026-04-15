use chrono::{DateTime, Utc};
use episcience_core::{Quantity, Sample, SampleStatus, SampleType};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::errors::DbError;

pub struct SampleRepository;

impl SampleRepository {
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
    ) -> Result<Sample, DbError> {
        let id = Uuid::now_v7();
        let now = Utc::now();
        let (q_val, q_unit) = match quantity {
            Some(q) => (Some(q.value), Some(q.unit.as_str())),
            None => (None, None),
        };

        let row = sqlx::query(
            r#"
            INSERT INTO samples (id, name, sample_type, status, parent_sample_id,
                prepared_by, preparation_date, storage_location,
                quantity_value, quantity_unit, hazard_info, labels, properties,
                content_hash, created_at, updated_at)
            VALUES ($1, $2, $3, 'prepared', $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $6, $6)
            RETURNING id, name, sample_type, status, parent_sample_id,
                prepared_by, preparation_date, expiry_date, storage_location,
                quantity_value, quantity_unit, hazard_info, labels, properties,
                content_hash, created_at, updated_at
            "#,
        )
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
        .fetch_one(pool)
        .await?;

        Ok(row_to_sample(&row))
    }

    pub async fn get_by_id(pool: &PgPool, id: Uuid) -> Result<Sample, DbError> {
        let row = sqlx::query(
            r#"
            SELECT id, name, sample_type, status, parent_sample_id,
                prepared_by, preparation_date, expiry_date, storage_location,
                quantity_value, quantity_unit, hazard_info, labels, properties,
                content_hash, created_at, updated_at
            FROM samples WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| DbError::NotFound {
            entity: "sample".into(),
            id: id.to_string(),
        })?;

        Ok(row_to_sample(&row))
    }

    pub async fn list(
        pool: &PgPool,
        status: Option<&str>,
        sample_type: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Sample>, DbError> {
        let rows = sqlx::query(
            r#"
            SELECT id, name, sample_type, status, parent_sample_id,
                prepared_by, preparation_date, expiry_date, storage_location,
                quantity_value, quantity_unit, hazard_info, labels, properties,
                content_hash, created_at, updated_at
            FROM samples
            WHERE ($1::text IS NULL OR status = $1)
              AND ($2::text IS NULL OR sample_type = $2)
            ORDER BY created_at DESC
            LIMIT $3 OFFSET $4
            "#,
        )
        .bind(status)
        .bind(sample_type)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;

        Ok(rows.iter().map(row_to_sample).collect())
    }

    pub async fn update_status(
        pool: &PgPool,
        id: Uuid,
        new_status: SampleStatus,
    ) -> Result<Sample, DbError> {
        let row = sqlx::query(
            r#"
            UPDATE samples SET status = $2, updated_at = NOW()
            WHERE id = $1
            RETURNING id, name, sample_type, status, parent_sample_id,
                prepared_by, preparation_date, expiry_date, storage_location,
                quantity_value, quantity_unit, hazard_info, labels, properties,
                content_hash, created_at, updated_at
            "#,
        )
        .bind(id)
        .bind(new_status.as_str())
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| DbError::NotFound {
            entity: "sample".into(),
            id: id.to_string(),
        })?;

        Ok(row_to_sample(&row))
    }

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
}

fn row_to_sample(row: &sqlx::postgres::PgRow) -> Sample {
    let quantity = match (
        row.get::<Option<f64>, _>("quantity_value"),
        row.get::<Option<String>, _>("quantity_unit"),
    ) {
        (Some(v), Some(u)) => Some(Quantity { value: v, unit: u }),
        _ => None,
    };

    Sample {
        id: row.get("id"),
        name: row.get("name"),
        sample_type: row
            .get::<String, _>("sample_type")
            .parse()
            .unwrap_or(SampleType::Material),
        status: row
            .get::<String, _>("status")
            .parse()
            .unwrap_or(SampleStatus::Prepared),
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
    }
}
