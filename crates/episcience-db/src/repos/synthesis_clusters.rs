use episcience_core::synthesis::Cluster;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::errors::DbError;

pub struct SynthesisClustersRepository;

impl SynthesisClustersRepository {
    pub async fn insert(pool: &PgPool, cluster: &Cluster) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO synthesis_clusters
             (id, synthesis_id, cluster_index, title, summary, member_claim_ids,
              support_count, contradict_count)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(cluster.id)
        .bind(cluster.synthesis_id)
        .bind(cluster.cluster_index)
        .bind(&cluster.title)
        .bind(&cluster.summary)
        .bind(&cluster.member_claim_ids)
        .bind(cluster.support_count)
        .bind(cluster.contradict_count)
        .execute(pool)
        .await?;
        Ok(())
    }

    pub async fn list_by_synthesis(
        pool: &PgPool,
        synthesis_id: Uuid,
    ) -> Result<Vec<Cluster>, DbError> {
        let rows = sqlx::query(
            "SELECT id, synthesis_id, cluster_index, title, summary, member_claim_ids,
             support_count, contradict_count
             FROM synthesis_clusters WHERE synthesis_id = $1
             ORDER BY cluster_index",
        )
        .bind(synthesis_id)
        .fetch_all(pool)
        .await?;

        rows.iter()
            .map(|r| {
                Ok(Cluster {
                    id: r.get("id"),
                    synthesis_id: r.get("synthesis_id"),
                    cluster_index: r.get("cluster_index"),
                    title: r.get("title"),
                    summary: r.get("summary"),
                    member_claim_ids: r.get("member_claim_ids"),
                    support_count: r.get("support_count"),
                    contradict_count: r.get("contradict_count"),
                })
            })
            .collect()
    }
}
