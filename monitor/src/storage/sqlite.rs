use anyhow::Result;
use chrono::DateTime;
use sqlx::SqlitePool;

pub struct Storage {
    pool: SqlitePool,
}

impl Storage {
    pub async fn new(database_url: &str) -> Result<Self> {
        let pool = SqlitePool::connect(database_url).await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS hourly_stats (
                hour TEXT PRIMARY KEY,
                stream_a_count INTEGER NOT NULL DEFAULT 0,
                stream_b_count INTEGER NOT NULL DEFAULT 0,
                delta INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&pool)
        .await?;

        Ok(Self { pool })
    }

    pub async fn save_hourly(
        &self,
        hour: DateTime<chrono::Utc>,
        stream_a: u64,
        stream_b: u64,
    ) -> Result<()> {
        let hour_str = hour.format("%Y-%m-%d %H:00:00").to_string();
        let delta = stream_a as i64 - stream_b as i64;

        sqlx::query(
            r#"
            INSERT INTO hourly_stats (hour, stream_a_count, stream_b_count, delta)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(hour) DO UPDATE SET
                stream_a_count = excluded.stream_a_count,
                stream_b_count = excluded.stream_b_count,
                delta = excluded.delta
            "#,
        )
        .bind(&hour_str)
        .bind(stream_a as i64)
        .bind(stream_b as i64)
        .bind(delta)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}
