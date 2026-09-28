use epigraph_db::Viewer;
use sqlx::Row;
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
    /// Full-text search over claims using the tsvector index, AS `viewer`.
    ///
    /// The statement carries the kernel's `/* {VISIBILITY:c} */` splice, so it
    /// returns exactly the claims the kernel would show the caller (public, or
    /// owned by one of its groups), never another owner's group-owned claim.
    pub async fn fulltext_search<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        viewer: &Viewer,
        query: &str,
        limit: i64,
    ) -> Result<Vec<FullTextResult>, DbError> {
        let sql = viewer.splice(
            r#"
            SELECT
                c.id,
                c.content,
                ts_rank(c.content_tsv, plainto_tsquery('english', $1)) AS rank
            FROM claims c
            WHERE c.content_tsv @@ plainto_tsquery('english', $1)
              /* {VISIBILITY:c} */
            ORDER BY rank DESC
            LIMIT $2
            "#,
            3,
        );
        let mut q = sqlx::query(&sql).bind(query).bind(limit);
        if let Some(groups) = viewer.group_bind() {
            q = q.bind(groups);
        }
        let rows = q.fetch_all(executor).await?;

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
