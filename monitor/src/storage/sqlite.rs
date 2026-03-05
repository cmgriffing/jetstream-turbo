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
    pub stream_a_downtime_seconds: i64,
    pub stream_b_downtime_seconds: i64,
    pub stream_a_disconnects: i64,
    pub stream_b_disconnects: i64,
    pub stream_a_connect_time_ms: i64,
    pub stream_b_connect_time_ms: i64,
    pub stream_a_messages: i64,
    pub stream_b_messages: i64,
    pub stream_a_delivery_latency_ms: f64,
    pub stream_b_delivery_latency_ms: f64,
    pub stream_a_mttr_ms: i64,
    pub stream_b_mttr_ms: i64,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct HourlyUptimeSimple {
    pub hour: String,
    pub stream_a_seconds: i64,
    pub stream_b_seconds: i64,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct LifetimeTotals {
    pub id: i64,
    pub stream_a_messages: i64,
    pub stream_b_messages: i64,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UptimeResponse {
    pub data: Vec<HourlyUptime>,
    pub span_seconds: i64,
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
                stream_a_downtime_seconds INTEGER NOT NULL DEFAULT 0,
                stream_b_downtime_seconds INTEGER NOT NULL DEFAULT 0,
                stream_a_disconnects INTEGER NOT NULL DEFAULT 0,
                stream_b_disconnects INTEGER NOT NULL DEFAULT 0,
                stream_a_connect_time_ms INTEGER NOT NULL DEFAULT 0,
                stream_b_connect_time_ms INTEGER NOT NULL DEFAULT 0,
                stream_a_messages INTEGER NOT NULL DEFAULT 0,
                stream_b_messages INTEGER NOT NULL DEFAULT 0,
                stream_a_delivery_latency_ms REAL NOT NULL DEFAULT 0.0,
                stream_b_delivery_latency_ms REAL NOT NULL DEFAULT 0.0,
                stream_a_mttr_ms INTEGER NOT NULL DEFAULT 0,
                stream_b_mttr_ms INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&pool)
        .await?;

        // Migration: add columns if they don't exist (for existing databases)
        sqlx::query(
            "ALTER TABLE hourly_uptime ADD COLUMN stream_a_downtime_seconds INTEGER NOT NULL DEFAULT 0"
        )
        .execute(&pool)
        .await
        .ok();

        sqlx::query(
            "ALTER TABLE hourly_uptime ADD COLUMN stream_b_downtime_seconds INTEGER NOT NULL DEFAULT 0"
        )
        .execute(&pool)
        .await
        .ok();

        sqlx::query(
            "ALTER TABLE hourly_uptime ADD COLUMN stream_a_delivery_latency_ms REAL NOT NULL DEFAULT 0.0"
        )
        .execute(&pool)
        .await
        .ok();

        sqlx::query(
            "ALTER TABLE hourly_uptime ADD COLUMN stream_b_delivery_latency_ms REAL NOT NULL DEFAULT 0.0"
        )
        .execute(&pool)
        .await
        .ok();

        sqlx::query(
            "ALTER TABLE hourly_uptime ADD COLUMN stream_a_mttr_ms INTEGER NOT NULL DEFAULT 0"
        )
        .execute(&pool)
        .await
        .ok();

        sqlx::query(
            "ALTER TABLE hourly_uptime ADD COLUMN stream_b_mttr_ms INTEGER NOT NULL DEFAULT 0"
        )
        .execute(&pool)
        .await
        .ok();

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS lifetime_totals (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                stream_a_messages INTEGER NOT NULL DEFAULT 0,
                stream_b_messages INTEGER NOT NULL DEFAULT 0,
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
            "#,
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
        stream_a_downtime_seconds: u64,
        stream_b_downtime_seconds: u64,
        stream_a_disconnects: u64,
        stream_b_disconnects: u64,
        stream_a_connect_time_ms: u64,
        stream_b_connect_time_ms: u64,
        stream_a_messages: u64,
        stream_b_messages: u64,
        stream_a_delivery_latency_ms: f64,
        stream_b_delivery_latency_ms: f64,
        stream_a_mttr_ms: u64,
        stream_b_mttr_ms: u64,
    ) -> Result<()> {
        let hour_str = hour.format("%Y-%m-%d %H:00:00").to_string();

        sqlx::query(
            r#"
            INSERT INTO hourly_uptime (
                hour, stream_a_seconds, stream_b_seconds,
                stream_a_downtime_seconds, stream_b_downtime_seconds,
                stream_a_disconnects, stream_b_disconnects,
                stream_a_connect_time_ms, stream_b_connect_time_ms,
                stream_a_messages, stream_b_messages,
                stream_a_delivery_latency_ms, stream_b_delivery_latency_ms,
                stream_a_mttr_ms, stream_b_mttr_ms
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(hour) DO UPDATE SET
                stream_a_seconds = excluded.stream_a_seconds,
                stream_b_seconds = excluded.stream_b_seconds,
                stream_a_downtime_seconds = excluded.stream_a_downtime_seconds,
                stream_b_downtime_seconds = excluded.stream_b_downtime_seconds,
                stream_a_disconnects = excluded.stream_a_disconnects,
                stream_b_disconnects = excluded.stream_b_disconnects,
                stream_a_connect_time_ms = excluded.stream_a_connect_time_ms,
                stream_b_connect_time_ms = excluded.stream_b_connect_time_ms,
                stream_a_messages = excluded.stream_a_messages,
                stream_b_messages = excluded.stream_b_messages,
                stream_a_delivery_latency_ms = excluded.stream_a_delivery_latency_ms,
                stream_b_delivery_latency_ms = excluded.stream_b_delivery_latency_ms,
                stream_a_mttr_ms = excluded.stream_a_mttr_ms,
                stream_b_mttr_ms = excluded.stream_b_mttr_ms,
                updated_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(&hour_str)
        .bind(stream_a_seconds as i64)
        .bind(stream_b_seconds as i64)
        .bind(stream_a_downtime_seconds as i64)
        .bind(stream_b_downtime_seconds as i64)
        .bind(stream_a_disconnects as i64)
        .bind(stream_b_disconnects as i64)
        .bind(stream_a_connect_time_ms as i64)
        .bind(stream_b_connect_time_ms as i64)
        .bind(stream_a_messages as i64)
        .bind(stream_b_messages as i64)
        .bind(stream_a_delivery_latency_ms)
        .bind(stream_b_delivery_latency_ms)
        .bind(stream_a_mttr_ms as i64)
        .bind(stream_b_mttr_ms as i64)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_uptime_since(&self, since: DateTime<Utc>) -> Result<Vec<HourlyUptime>> {
        let since_str = since.format("%Y-%m-%d %H:00:00").to_string();

        let rows = sqlx::query_as::<_, HourlyUptime>(
            r#"
            SELECT hour, stream_a_seconds, stream_b_seconds,
                   stream_a_downtime_seconds, stream_b_downtime_seconds,
                   stream_a_disconnects, stream_b_disconnects,
                   stream_a_connect_time_ms, stream_b_connect_time_ms,
                   stream_a_messages, stream_b_messages,
                   stream_a_delivery_latency_ms, stream_b_delivery_latency_ms,
                   stream_a_mttr_ms, stream_b_mttr_ms
            FROM hourly_uptime
            WHERE hour >= ?
            ORDER BY hour ASC
            "#,
        )
        .bind(since_str)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    pub async fn save_lifetime_totals(
        &self,
        stream_a_messages: u64,
        stream_b_messages: u64,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO lifetime_totals (id, stream_a_messages, stream_b_messages)
            VALUES (1, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                stream_a_messages = excluded.stream_a_messages,
                stream_b_messages = excluded.stream_b_messages,
                updated_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(stream_a_messages as i64)
        .bind(stream_b_messages as i64)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_lifetime_totals(&self) -> Result<(u64, u64)> {
        let result = sqlx::query_as::<_, LifetimeTotals>(
            "SELECT * FROM lifetime_totals WHERE id = 1"
        )
        .fetch_optional(&self.pool)
        .await?;

        match result {
            Some(row) => Ok((row.stream_a_messages as u64, row.stream_b_messages as u64)),
            None => Ok((0, 0)),
        }
    }
}
