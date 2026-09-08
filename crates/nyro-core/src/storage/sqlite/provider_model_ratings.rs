use async_trait::async_trait;
use sqlx::SqlitePool;

use crate::db::models::ProviderModelRating;
use crate::db::provider_model_ratings::ensure_model_byte_limit;
use crate::storage::ProviderModelRatingStore;

pub(super) struct SqliteProviderModelRatingStore {
    pub(super) pool: SqlitePool,
}

const UPSERT: &str = "INSERT INTO provider_model_ratings \
    (provider_id, upstream_model, score, updated_at) VALUES (?, ?, ?, ?) \
    ON CONFLICT(provider_id, upstream_model) DO UPDATE SET \
    score = excluded.score, updated_at = excluded.updated_at";

#[async_trait]
impl ProviderModelRatingStore for SqliteProviderModelRatingStore {
    async fn list(&self, provider_id: Option<&str>) -> anyhow::Result<Vec<ProviderModelRating>> {
        let rows = if let Some(provider_id) = provider_id {
            sqlx::query_as::<_, ProviderModelRating>(
                "SELECT provider_id, upstream_model, score, updated_at \
                 FROM provider_model_ratings WHERE provider_id = ? ORDER BY upstream_model",
            )
            .bind(provider_id)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, ProviderModelRating>(
                "SELECT provider_id, upstream_model, score, updated_at \
                 FROM provider_model_ratings ORDER BY provider_id, upstream_model",
            )
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows)
    }

    async fn get(
        &self,
        provider_id: &str,
        model: &str,
    ) -> anyhow::Result<Option<ProviderModelRating>> {
        Ok(sqlx::query_as::<_, ProviderModelRating>(
            "SELECT provider_id, upstream_model, score, updated_at \
             FROM provider_model_ratings WHERE provider_id = ? AND upstream_model = ?",
        )
        .bind(provider_id)
        .bind(model)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn upsert(&self, rating: ProviderModelRating) -> anyhow::Result<ProviderModelRating> {
        ensure_model_byte_limit(&rating.upstream_model)?;
        // RETURNING observes this write, rather than a racing write after it.
        Ok(sqlx::query_as::<_, ProviderModelRating>(&format!(
            "{UPSERT} RETURNING provider_id, upstream_model, score, updated_at"
        ))
        .bind(&rating.provider_id)
        .bind(&rating.upstream_model)
        .bind(rating.score)
        .bind(&rating.updated_at)
        .fetch_one(&self.pool)
        .await?)
    }

    async fn delete(&self, provider_id: &str, model: &str) -> anyhow::Result<()> {
        sqlx::query(
            "DELETE FROM provider_model_ratings WHERE provider_id = ? AND upstream_model = ?",
        )
        .bind(provider_id)
        .bind(model)
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
        sqlx::query("DELETE FROM provider_model_ratings WHERE provider_id = ?")
            .bind(provider_id)
            .execute(&mut *tx)
            .await?;
        for rating in ratings {
            ensure_model_byte_limit(&rating.upstream_model)?;
            sqlx::query(UPSERT)
                .bind(provider_id)
                .bind(&rating.upstream_model)
                .bind(rating.score)
                .bind(&rating.updated_at)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
