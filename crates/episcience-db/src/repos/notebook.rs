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
    /// Full-text search over claims using the tsvector index, restricted to
    /// claims authored by `principal`.
    ///
    /// Interim read rule (batch E1a): this route reads kernel `claims`
    /// directly, outside the kernel's visibility rules, so it returns only the
    /// caller's own claims. The kernel viewer predicate replaces this filter
    /// when EpiScience moves to the tenancy-aware kernel pin.
    pub async fn fulltext_search(
        pool: &PgPool,
        query: &str,
        limit: i64,
        principal: Uuid,
    ) -> Result<Vec<FullTextResult>, DbError> {
        let rows = sqlx::query(
            r#"
            SELECT
                id,
                content,
                ts_rank(content_tsv, plainto_tsquery('english', $1)) AS rank
            FROM claims
            WHERE content_tsv @@ plainto_tsquery('english', $1)
              AND agent_id = $3
            ORDER BY rank DESC
            LIMIT $2
            "#,
        )
        .bind(query)
        .bind(limit)
        .bind(principal)
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
