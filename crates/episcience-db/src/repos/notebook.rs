use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::errors::DbError;

/// Full-text search result from tsvector index.
#[derive(Debug)]
pub struct FullTextResult {
    pub claim_id: Uuid,
    pub content: String,
    pub rank: f32,
}

pub struct NotebookRepository;

impl NotebookRepository {
    /// Full-text search over claims using the tsvector index.
    pub async fn fulltext_search(
        pool: &PgPool,
        query: &str,
        limit: i64,
    ) -> Result<Vec<FullTextResult>, DbError> {
        let rows = sqlx::query(
            r#"
            SELECT
                id,
                content,
                ts_rank(content_tsv, plainto_tsquery('english', $1)) AS rank
            FROM claims
            WHERE content_tsv @@ plainto_tsquery('english', $1)
            ORDER BY rank DESC
            LIMIT $2
            "#,
        )
        .bind(query)
        .bind(limit)
        .fetch_all(pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| FullTextResult {
                claim_id: r.get("id"),
                content: r.get("content"),
                rank: r.get("rank"),
            })
            .collect())
    }
}
