use epigraph_db::Viewer;
use episcience_core::{Ownership, Protocol, ProtocolSections, ProtocolStep, Visibility};
use sqlx::Row;
use uuid::Uuid;

use crate::errors::DbError;

pub struct ProtocolRepository;

impl ProtocolRepository {
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        conn: &mut sqlx::PgConnection,
        title: &str,
        authored_by: Uuid,
        steps: &[ProtocolStep],
        equipment: &[String],
        safety_notes: Option<&str>,
        supersedes: Option<Uuid>,
        labels: &[String],
        properties: &serde_json::Value,
        content_hash: &[u8],
        sections: &ProtocolSections,
        owner: Ownership,
    ) -> Result<Protocol, DbError> {
        let id = Uuid::now_v7();
        let steps_json = serde_json::to_value(steps)
            .map_err(|e| DbError::Serialization(format!("serialize steps: {e}")))?;
        let sections_json = serde_json::to_value(sections)
            .map_err(|e| DbError::Serialization(format!("serialize sections: {e}")))?;

        let mut tx = sqlx::Connection::begin(&mut *conn).await?;

        let version: i32 = if let Some(prev_id) = supersedes {
            let row = sqlx::query("SELECT version FROM protocols WHERE id = $1 FOR UPDATE")
                .bind(prev_id)
                .fetch_optional(&mut *tx)
                .await?;
            row.map(|r| r.get::<i32, _>("version") + 1)
                .ok_or_else(|| DbError::NotFound {
                    entity: "protocol".into(),
                    id: prev_id.to_string(),
                })?
        } else {
            1
        };

        let row = sqlx::query(
            r#"
            INSERT INTO protocols (id, title, version, authored_by, steps, equipment,
                safety_notes, supersedes, labels, properties, content_hash, sections,
                created_at, updated_at, owner_group_id, visibility)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, NOW(), NOW(), $13, $14)
            RETURNING id, title, version, authored_by, steps, equipment,
                safety_notes, supersedes, labels, properties, content_hash, sections,
                created_at, updated_at, owner_group_id, visibility
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
        .bind(&sections_json)
        .bind(owner.owner_group_id)
        .bind(owner.visibility.as_str())
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;

        row_to_protocol(&row)
    }

    /// The protocol, UNFILTERED (internal use; request handlers use
    /// [`Self::get_readable`]).
    pub async fn get_by_id<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        id: Uuid,
    ) -> Result<Protocol, DbError> {
        let row = sqlx::query(
            r#"
            SELECT id, title, version, authored_by, steps, equipment,
                safety_notes, supersedes, labels, properties, content_hash, sections,
                created_at, updated_at, owner_group_id, visibility
            FROM protocols WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(executor)
        .await?
        .ok_or_else(|| DbError::NotFound {
            entity: "protocol".into(),
            id: id.to_string(),
        })?;

        row_to_protocol(&row)
    }

    /// The protocol if `viewer` can read it; an invisible protocol is
    /// reported exactly like a missing one.
    pub async fn get_readable<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        id: Uuid,
        viewer: &Viewer,
    ) -> Result<Protocol, DbError> {
        let sql = viewer.splice(
            "SELECT p.id, p.title, p.version, p.authored_by, p.steps, p.equipment,
                    p.safety_notes, p.supersedes, p.labels, p.properties, p.content_hash, p.sections,
                    p.created_at, p.updated_at, p.owner_group_id, p.visibility
               FROM protocols p WHERE p.id = $1 /* {VISIBILITY:p} */",
            2,
        );
        let mut q = sqlx::query(&sql).bind(id);
        if let Some(groups) = viewer.group_bind() {
            q = q.bind(groups);
        }
        let row = q
            .fetch_optional(executor)
            .await?
            .ok_or_else(|| DbError::NotFound {
                entity: "protocol".into(),
                id: id.to_string(),
            })?;
        row_to_protocol(&row)
    }

    /// Whether `viewer` may supersede (edit) protocol `id`: it is owned by
    /// one of the viewer's writable groups. A new version of a protocol the
    /// caller cannot edit is a FORK: a new root, not a supersede.
    pub async fn writable_by<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        id: Uuid,
        viewer: &Viewer,
    ) -> Result<bool, DbError> {
        let sql = viewer.splice_write(
            "SELECT EXISTS (SELECT 1 FROM protocols p WHERE p.id = $1 /* {WRITABLE:p} */)",
            2,
        );
        let mut q = sqlx::query_scalar::<_, bool>(&sql).bind(id);
        if let Some(groups) = viewer.writable_bind() {
            q = q.bind(groups);
        }
        Ok(q.fetch_one(executor).await?)
    }
}

fn row_to_protocol(row: &sqlx::postgres::PgRow) -> Result<Protocol, DbError> {
    let steps_json: serde_json::Value = row.get("steps");
    let steps: Vec<ProtocolStep> = serde_json::from_value(steps_json)
        .map_err(|e| DbError::Serialization(format!("deserialize steps: {e}")))?;

    let sections_json: serde_json::Value = row.get("sections");
    let sections: ProtocolSections = serde_json::from_value(sections_json)
        .map_err(|e| DbError::Serialization(format!("deserialize sections: {e}")))?;

    Ok(Protocol {
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
        sections,
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
