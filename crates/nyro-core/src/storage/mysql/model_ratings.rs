use async_trait::async_trait;
use sqlx::{MySqlPool, Row, mysql::MySqlRow};

use crate::db::model_rating_prefixes::{ModelRatingEntry, ensure_rating_entry};
use crate::storage::ModelRatingStore;

pub(super) struct MysqlModelRatingStore {
    pub(super) pool: MySqlPool,
}

const UPSERT: &str = "INSERT INTO model_rating_prefixes \
    (model_prefix, score, updated_at) VALUES (?, ?, ?) \
    ON DUPLICATE KEY UPDATE score = VALUES(score), updated_at = VALUES(updated_at)";

/// VARBINARY keys are decoded explicitly; invalid UTF-8 is an error, never
/// replacement text.
fn decode_entry(row: MySqlRow) -> anyhow::Result<ModelRatingEntry> {
    Ok(ModelRatingEntry {
        model_prefix: String::from_utf8(row.try_get::<Vec<u8>, _>("model_prefix")?)?,
        score: row.try_get("score")?,
        updated_at: row.try_get("updated_at")?,
    })
}

#[async_trait]
impl ModelRatingStore for MysqlModelRatingStore {
    async fn list(&self) -> anyhow::Result<Vec<ModelRatingEntry>> {
        let rows = sqlx::query(
            "SELECT model_prefix, score, updated_at FROM model_rating_prefixes ORDER BY model_prefix",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(decode_entry).collect()
    }

    async fn upsert(&self, entry: ModelRatingEntry) -> anyhow::Result<ModelRatingEntry> {
        ensure_rating_entry(&entry)?;
        sqlx::query(UPSERT)
            .bind(entry.model_prefix.as_bytes())
            .bind(entry.score)
            .bind(&entry.updated_at)
            .execute(&self.pool)
            .await?;
        Ok(entry)
    }

    async fn delete(&self, model_prefix: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM model_rating_prefixes WHERE model_prefix = ?")
            .bind(model_prefix.as_bytes())
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
                .bind(entry.model_prefix.as_bytes())
                .bind(entry.score)
                .bind(&entry.updated_at)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
