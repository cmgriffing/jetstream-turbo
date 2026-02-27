use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, SqlitePool};

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct HourlyStat {
    pub hour: String,
    pub stream_a_count: i64,
    pub stream_b_count: i64,
    pub delta: i64,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct HourlyUptime {
    pub hour: String,
    pub stream_a_seconds: i64,
    pub stream_b_seconds: i64,
}

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

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS hourly_uptime (
                hour TEXT PRIMARY KEY,
                stream_a_seconds INTEGER NOT NULL DEFAULT 0,
                stream_b_seconds INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
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

    pub async fn get_stats_since(&self, since: DateTime<Utc>) -> Result<Vec<HourlyStat>> {
        let since_str = since.format("%Y-%m-%d %H:00:00").to_string();

        let rows = sqlx::query_as::<_, HourlyStat>(
            r#"
            SELECT hour, stream_a_count, stream_b_count, delta
            FROM hourly_stats
            WHERE hour >= ?
            ORDER BY hour ASC
            "#
        )
        .bind(since_str)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    pub async fn save_hourly_uptime(
        &self,
        hour: DateTime<Utc>,
        stream_a_seconds: u64,
        stream_b_seconds: u64,
    ) -> Result<()> {
        let hour_str = hour.format("%Y-%m-%d %H:00:00").to_string();

        sqlx::query(
            r#"
            INSERT INTO hourly_uptime (hour, stream_a_seconds, stream_b_seconds)
            VALUES (?, ?, ?)
            ON CONFLICT(hour) DO UPDATE SET
                stream_a_seconds = excluded.stream_a_seconds,
                stream_b_seconds = excluded.stream_b_seconds,
                updated_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(&hour_str)
        .bind(stream_a_seconds as i64)
        .bind(stream_b_seconds as i64)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_uptime_since(&self, since: DateTime<Utc>) -> Result<Vec<HourlyUptime>> {
        let since_str = since.format("%Y-%m-%d %H:00:00").to_string();

        let rows = sqlx::query_as::<_, HourlyUptime>(
            r#"
            SELECT hour, stream_a_seconds, stream_b_seconds
            FROM hourly_uptime
            WHERE hour >= ?
            ORDER BY hour ASC
            "#
        )
        .bind(since_str)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }
}
