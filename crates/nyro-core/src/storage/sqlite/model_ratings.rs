use async_trait::async_trait;
use sqlx::SqlitePool;

use crate::db::model_rating_prefixes::{ModelRatingEntry, ensure_rating_entry};
use crate::storage::ModelRatingStore;

pub(super) struct SqliteModelRatingStore {
    pub(super) pool: SqlitePool,
}

const UPSERT: &str = "INSERT INTO model_rating_prefixes \
    (model_prefix, score, updated_at) VALUES (?, ?, ?) \
    ON CONFLICT(model_prefix) DO UPDATE SET \
    score = excluded.score, updated_at = excluded.updated_at";

#[async_trait]
impl ModelRatingStore for SqliteModelRatingStore {
    async fn list(&self) -> anyhow::Result<Vec<ModelRatingEntry>> {
        Ok(sqlx::query_as::<_, ModelRatingEntry>(
            "SELECT model_prefix, score, updated_at FROM model_rating_prefixes ORDER BY model_prefix",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    async fn upsert(&self, entry: ModelRatingEntry) -> anyhow::Result<ModelRatingEntry> {
        ensure_rating_entry(&entry)?;
        Ok(sqlx::query_as::<_, ModelRatingEntry>(&format!(
            "{UPSERT} RETURNING model_prefix, score, updated_at"
        ))
        .bind(&entry.model_prefix)
        .bind(entry.score)
        .bind(&entry.updated_at)
        .fetch_one(&self.pool)
        .await?)
    }

    async fn delete(&self, model_prefix: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM model_rating_prefixes WHERE model_prefix = ?")
            .bind(model_prefix)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn restore(&self, entries: &[ModelRatingEntry]) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM model_rating_prefixes")
            .execute(&mut *tx)
            .await?;
        for entry in entries {
            ensure_rating_entry(entry)?;
            sqlx::query(UPSERT)
                .bind(&entry.model_prefix)
                .bind(entry.score)
                .bind(&entry.updated_at)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
