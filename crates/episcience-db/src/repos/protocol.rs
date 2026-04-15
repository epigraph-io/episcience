use episcience_core::{Protocol, ProtocolStep};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::errors::DbError;

pub struct ProtocolRepository;

impl ProtocolRepository {
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        pool: &PgPool,
        title: &str,
        authored_by: Uuid,
        steps: &[ProtocolStep],
        equipment: &[String],
        safety_notes: Option<&str>,
        supersedes: Option<Uuid>,
        labels: &[String],
        properties: &serde_json::Value,
        content_hash: &[u8],
    ) -> Result<Protocol, DbError> {
        let id = Uuid::now_v7();
        let steps_json = serde_json::to_value(steps).unwrap_or_default();

        let version: i32 = if let Some(prev_id) = supersedes {
            let row = sqlx::query("SELECT version FROM protocols WHERE id = $1")
                .bind(prev_id)
                .fetch_optional(pool)
                .await?;
            row.map(|r| r.get::<i32, _>("version") + 1).unwrap_or(1)
        } else {
            1
        };

        let row = sqlx::query(
            r#"
            INSERT INTO protocols (id, title, version, authored_by, steps, equipment,
                safety_notes, supersedes, labels, properties, content_hash,
                created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, NOW(), NOW())
            RETURNING id, title, version, authored_by, steps, equipment,
                safety_notes, supersedes, labels, properties, content_hash,
                created_at, updated_at
            "#,
        )
        .bind(id)
        .bind(title)
        .bind(version)
        .bind(authored_by)
        .bind(&steps_json)
        .bind(equipment)
        .bind(safety_notes)
        .bind(supersedes)
        .bind(labels)
        .bind(properties)
        .bind(content_hash)
        .fetch_one(pool)
        .await?;

        Ok(row_to_protocol(&row))
    }

    pub async fn get_by_id(pool: &PgPool, id: Uuid) -> Result<Protocol, DbError> {
        let row = sqlx::query(
            r#"
            SELECT id, title, version, authored_by, steps, equipment,
                safety_notes, supersedes, labels, properties, content_hash,
                created_at, updated_at
            FROM protocols WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| DbError::NotFound {
            entity: "protocol".into(),
            id: id.to_string(),
        })?;

        Ok(row_to_protocol(&row))
    }
}

fn row_to_protocol(row: &sqlx::postgres::PgRow) -> Protocol {
    let steps_json: serde_json::Value = row.get("steps");
    let steps: Vec<ProtocolStep> = serde_json::from_value(steps_json).unwrap_or_default();

    Protocol {
        id: row.get("id"),
        title: row.get("title"),
        version: row.get("version"),
        authored_by: row.get("authored_by"),
        steps,
        equipment: row.get("equipment"),
        safety_notes: row.get("safety_notes"),
        supersedes: row.get("supersedes"),
        labels: row.get("labels"),
        properties: row.get("properties"),
        content_hash: row.get("content_hash"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}
