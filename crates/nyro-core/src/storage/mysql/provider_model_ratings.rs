use async_trait::async_trait;
use sqlx::{MySqlPool, Row, mysql::MySqlRow};

use crate::db::models::ProviderModelRating;
use crate::db::provider_model_ratings::ensure_rating;
use crate::storage::ProviderModelRatingStore;

pub(super) struct MysqlProviderModelRatingStore {
    pub(super) pool: MySqlPool,
}

const UPSERT: &str = "INSERT INTO provider_model_ratings \
    (provider_id, upstream_model, effort, score, updated_at) VALUES (?, ?, 'common', ?, ?) \
    ON DUPLICATE KEY UPDATE score = VALUES(score), updated_at = VALUES(updated_at)";
const GET: &str = "SELECT provider_id, upstream_model, score, updated_at \
    FROM provider_model_ratings WHERE provider_id = ? AND upstream_model = ? AND effort = 'common'";

/// VARBINARY keys are decoded explicitly; invalid UTF-8 is an error, never replacement text.
fn decode_rating(row: MySqlRow) -> anyhow::Result<ProviderModelRating> {
    Ok(ProviderModelRating {
        provider_id: row.try_get("provider_id")?,
        upstream_model: String::from_utf8(row.try_get("upstream_model")?)?,
        score: row.try_get("score")?,
        updated_at: row.try_get("updated_at")?,
    })
}

#[async_trait]
impl ProviderModelRatingStore for MysqlProviderModelRatingStore {
    async fn list(&self, provider_id: Option<&str>) -> anyhow::Result<Vec<ProviderModelRating>> {
        let rows = if let Some(provider_id) = provider_id {
            sqlx::query(
                "SELECT provider_id, upstream_model, score, updated_at \
                 FROM provider_model_ratings WHERE provider_id = ? AND effort = 'common' ORDER BY upstream_model",
            )
            .bind(provider_id)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT provider_id, upstream_model, score, updated_at \
                 FROM provider_model_ratings WHERE effort = 'common' ORDER BY provider_id, upstream_model",
            )
            .fetch_all(&self.pool)
            .await?
        };
        rows.into_iter().map(decode_rating).collect()
    }

    async fn get(
        &self,
        provider_id: &str,
        model: &str,
    ) -> anyhow::Result<Option<ProviderModelRating>> {
        sqlx::query(GET)
            .bind(provider_id)
            .bind(model.as_bytes())
            .fetch_optional(&self.pool)
            .await?
            .map(decode_rating)
            .transpose()
    }

    async fn upsert(&self, rating: ProviderModelRating) -> anyhow::Result<ProviderModelRating> {
        ensure_rating(&rating)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(UPSERT)
            .bind(&rating.provider_id)
            .bind(rating.upstream_model.as_bytes())
            .bind(rating.score)
            .bind(&rating.updated_at)
            .execute(&mut *tx)
            .await?;
        // Keep the write lock through the read so the response represents this write.
        let saved = decode_rating(
            sqlx::query(GET)
                .bind(&rating.provider_id)
                .bind(rating.upstream_model.as_bytes())
                .fetch_one(&mut *tx)
                .await?,
        )?;
        tx.commit().await?;
        Ok(saved)
    }

    async fn delete(&self, provider_id: &str, model: &str) -> anyhow::Result<()> {
        sqlx::query(
            "DELETE FROM provider_model_ratings WHERE provider_id = ? AND upstream_model = ? AND effort = 'common'",
        )
        .bind(provider_id)
        .bind(model.as_bytes())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn restore(
        &self,
        provider_id: &str,
        ratings: &[ProviderModelRating],
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT id FROM providers WHERE id = ? FOR UPDATE")
            .bind(provider_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "DELETE FROM provider_model_ratings WHERE provider_id = ? AND effort = 'common'",
        )
        .bind(provider_id)
        .execute(&mut *tx)
        .await?;
        for rating in ratings {
            ensure_rating(rating)?;
            sqlx::query(UPSERT)
                .bind(provider_id)
                .bind(rating.upstream_model.as_bytes())
                .bind(rating.score)
                .bind(&rating.updated_at)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
