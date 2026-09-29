use epigraph_db::Viewer;
use sqlx::Row;
use uuid::Uuid;

use crate::errors::DbError;

/// Encode a slice of f32 as a pgvector text literal, e.g. "[1.0,2.0,3.0]".
/// We pass this as TEXT and cast with `$1::text::vector` or use
/// `string_to_array` — simplest is to just pass the text representation
/// and cast in SQL as `$1::vector` (Postgres accepts the vector text format).
fn vec_to_text(values: &[f32]) -> String {
    let inner: Vec<String> = values.iter().map(|v| format!("{v}")).collect();
    format!("[{}]", inner.join(","))
}

pub struct SynthesisEmbeddingsRepository;

impl SynthesisEmbeddingsRepository {
    pub async fn upsert<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        synthesis_id: Uuid,
        embedding: &[f32],
        model: &str,
        input_kind: &str,
    ) -> Result<(), DbError> {
        let text = vec_to_text(embedding);
        sqlx::query(
            "INSERT INTO synthesis_embeddings
             (synthesis_id, embedding, embedding_model, embedding_input)
             VALUES ($1, $2::vector, $3, $4)
             ON CONFLICT (synthesis_id) DO UPDATE
             SET embedding = EXCLUDED.embedding,
                 embedding_model = EXCLUDED.embedding_model,
                 embedding_input = EXCLUDED.embedding_input,
                 created_at = now()",
        )
        .bind(synthesis_id)
        .bind(text)
        .bind(model)
        .bind(input_kind)
        .execute(executor)
        .await?;
        Ok(())
    }

    pub async fn exists<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        synthesis_id: Uuid,
    ) -> Result<bool, DbError> {
        let result = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM synthesis_embeddings WHERE synthesis_id = $1)",
        )
        .bind(synthesis_id)
        .fetch_one(executor)
        .await?;
        Ok(result)
    }

    /// Nearest syntheses to `query_embedding` among those `viewer` can read
    /// (public, or owned by one of the viewer's groups: the kernel's
    /// `Viewer::splice` on the synthesis row). Stale rows only when
    /// `include_stale`.
    pub async fn search<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        query_embedding: &[f32],
        limit: usize,
        min_score: f64,
        viewer: &Viewer,
        include_stale: bool,
    ) -> Result<Vec<(Uuid, f64)>, DbError> {
        let text = vec_to_text(query_embedding);
        let sql = viewer.splice(
            "SELECT se.synthesis_id,
                    1 - (se.embedding <=> $1::vector) AS score
             FROM synthesis_embeddings se
             JOIN syntheses s ON s.id = se.synthesis_id
             WHERE ($4 OR s.stale_since IS NULL)
               AND (1 - (se.embedding <=> $1::vector)) >= $2
               /* {VISIBILITY:s} */
             ORDER BY se.embedding <=> $1::vector
             LIMIT $3",
            5,
        );
        let mut q = sqlx::query(&sql)
            .bind(text)
            .bind(min_score)
            .bind(limit as i64)
            .bind(include_stale);
        if let Some(groups) = viewer.group_bind() {
            q = q.bind(groups);
        }
        let rows = q.fetch_all(executor).await?;

        rows.iter()
            .map(|r| {
                let id: Uuid = r.get("synthesis_id");
                let score: f64 = r.get("score");
                Ok((id, score))
            })
            .collect()
    }
}
