use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, SqlitePool};

const LEGACY_UPTIME_CONTRACT_VERSION: i64 = 1;
const INTERVAL_UPTIME_CONTRACT_VERSION: i64 = 2;
const HOURLY_WINDOW_SECONDS: i64 = 3600;

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
    #[serde(skip_serializing)]
    pub metrics_contract_version: i64,
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
    pub requested_window_seconds: i64,
    pub interval_seconds: i64,
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
                metrics_contract_version INTEGER NOT NULL DEFAULT 1,
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
            "ALTER TABLE hourly_uptime ADD COLUMN stream_a_disconnects INTEGER NOT NULL DEFAULT 0",
        )
        .execute(&pool)
        .await
        .ok();

        sqlx::query(
            "ALTER TABLE hourly_uptime ADD COLUMN stream_b_disconnects INTEGER NOT NULL DEFAULT 0",
        )
        .execute(&pool)
        .await
        .ok();

        sqlx::query(
            "ALTER TABLE hourly_uptime ADD COLUMN stream_a_connect_time_ms INTEGER NOT NULL DEFAULT 0",
        )
        .execute(&pool)
        .await
        .ok();

        sqlx::query(
            "ALTER TABLE hourly_uptime ADD COLUMN stream_b_connect_time_ms INTEGER NOT NULL DEFAULT 0",
        )
        .execute(&pool)
        .await
        .ok();

        sqlx::query(
            "ALTER TABLE hourly_uptime ADD COLUMN stream_a_messages INTEGER NOT NULL DEFAULT 0",
        )
        .execute(&pool)
        .await
        .ok();

        sqlx::query(
            "ALTER TABLE hourly_uptime ADD COLUMN stream_b_messages INTEGER NOT NULL DEFAULT 0",
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
            "ALTER TABLE hourly_uptime ADD COLUMN stream_a_mttr_ms INTEGER NOT NULL DEFAULT 0",
        )
        .execute(&pool)
        .await
        .ok();

        sqlx::query(
            "ALTER TABLE hourly_uptime ADD COLUMN stream_b_mttr_ms INTEGER NOT NULL DEFAULT 0",
        )
        .execute(&pool)
        .await
        .ok();

        sqlx::query(
            "ALTER TABLE hourly_uptime ADD COLUMN metrics_contract_version INTEGER NOT NULL DEFAULT 1"
        )
        .execute(&pool)
        .await
        .ok();

        sqlx::query("ALTER TABLE hourly_uptime ADD COLUMN updated_at TEXT NOT NULL DEFAULT ''")
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
        metrics_contract_version: i64,
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
                stream_a_mttr_ms, stream_b_mttr_ms,
                metrics_contract_version
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
                metrics_contract_version = excluded.metrics_contract_version,
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
        .bind(metrics_contract_version)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_uptime_since(&self, since: DateTime<Utc>) -> Result<Vec<HourlyUptime>> {
        let since_str = since.format("%Y-%m-%d %H:00:00").to_string();

        let previous_row = sqlx::query_as::<_, HourlyUptime>(
            r#"
            SELECT hour, stream_a_seconds, stream_b_seconds,
                   stream_a_downtime_seconds, stream_b_downtime_seconds,
                   stream_a_disconnects, stream_b_disconnects,
                   stream_a_connect_time_ms, stream_b_connect_time_ms,
                   stream_a_messages, stream_b_messages,
                   stream_a_delivery_latency_ms, stream_b_delivery_latency_ms,
                   stream_a_mttr_ms, stream_b_mttr_ms,
                   metrics_contract_version
            FROM hourly_uptime
            WHERE hour < ?
            ORDER BY hour DESC
            LIMIT 1
            "#,
        )
        .bind(&since_str)
        .fetch_optional(&self.pool)
        .await?;

        let rows = sqlx::query_as::<_, HourlyUptime>(
            r#"
            SELECT hour, stream_a_seconds, stream_b_seconds,
                   stream_a_downtime_seconds, stream_b_downtime_seconds,
                   stream_a_disconnects, stream_b_disconnects,
                   stream_a_connect_time_ms, stream_b_connect_time_ms,
                   stream_a_messages, stream_b_messages,
                   stream_a_delivery_latency_ms, stream_b_delivery_latency_ms,
                   stream_a_mttr_ms, stream_b_mttr_ms,
                   metrics_contract_version
            FROM hourly_uptime
            WHERE hour >= ?
            ORDER BY hour ASC
            "#,
        )
        .bind(&since_str)
        .fetch_all(&self.pool)
        .await?;

        Ok(Self::normalize_uptime_rows(rows, previous_row))
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
        let result =
            sqlx::query_as::<_, LifetimeTotals>("SELECT * FROM lifetime_totals WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?;

        match result {
            Some(row) => Ok((row.stream_a_messages as u64, row.stream_b_messages as u64)),
            None => Ok((0, 0)),
        }
    }

    fn counter_delta(current: i64, previous: i64) -> i64 {
        if current >= previous {
            current - previous
        } else {
            current.max(0)
        }
    }

    fn normalize_uptime_rows(
        rows: Vec<HourlyUptime>,
        baseline: Option<HourlyUptime>,
    ) -> Vec<HourlyUptime> {
        let mut normalized = Vec::with_capacity(rows.len());
        let mut previous_legacy =
            baseline.filter(|row| row.metrics_contract_version == LEGACY_UPTIME_CONTRACT_VERSION);

        for mut row in rows {
            let original = row.clone();

            if row.metrics_contract_version == LEGACY_UPTIME_CONTRACT_VERSION {
                let (downtime_a, downtime_b, disconnects_a, disconnects_b, messages_a, messages_b) =
                    if let Some(previous) = &previous_legacy {
                        (
                            Self::counter_delta(
                                row.stream_a_downtime_seconds,
                                previous.stream_a_downtime_seconds,
                            ),
                            Self::counter_delta(
                                row.stream_b_downtime_seconds,
                                previous.stream_b_downtime_seconds,
                            ),
                            Self::counter_delta(
                                row.stream_a_disconnects,
                                previous.stream_a_disconnects,
                            ),
                            Self::counter_delta(
                                row.stream_b_disconnects,
                                previous.stream_b_disconnects,
                            ),
                            Self::counter_delta(row.stream_a_messages, previous.stream_a_messages),
                            Self::counter_delta(row.stream_b_messages, previous.stream_b_messages),
                        )
                    } else {
                        (
                            row.stream_a_downtime_seconds.max(0),
                            row.stream_b_downtime_seconds.max(0),
                            row.stream_a_disconnects.max(0),
                            row.stream_b_disconnects.max(0),
                            row.stream_a_messages.max(0),
                            row.stream_b_messages.max(0),
                        )
                    };

                // Legacy rows stored downtime cumulatively and uptime as percentage-cast values.
                // Reconstruct interval uptime from interval downtime for 1h buckets.
                let downtime_a = downtime_a.clamp(0, HOURLY_WINDOW_SECONDS);
                let downtime_b = downtime_b.clamp(0, HOURLY_WINDOW_SECONDS);
                let uptime_a = (HOURLY_WINDOW_SECONDS - downtime_a).clamp(0, HOURLY_WINDOW_SECONDS);
                let uptime_b = (HOURLY_WINDOW_SECONDS - downtime_b).clamp(0, HOURLY_WINDOW_SECONDS);

                row.stream_a_seconds = uptime_a;
                row.stream_b_seconds = uptime_b;
                row.stream_a_downtime_seconds = downtime_a;
                row.stream_b_downtime_seconds = downtime_b;
                row.stream_a_disconnects = disconnects_a.max(0);
                row.stream_b_disconnects = disconnects_b.max(0);
                row.stream_a_messages = messages_a.max(0);
                row.stream_b_messages = messages_b.max(0);
                row.stream_a_connect_time_ms = row.stream_a_connect_time_ms.max(0);
                row.stream_b_connect_time_ms = row.stream_b_connect_time_ms.max(0);
                row.stream_a_mttr_ms = row.stream_a_mttr_ms.max(0);
                row.stream_b_mttr_ms = row.stream_b_mttr_ms.max(0);
                row.metrics_contract_version = INTERVAL_UPTIME_CONTRACT_VERSION;

                previous_legacy = Some(original);
            }

            normalized.push(row);
        }

        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::{Storage, INTERVAL_UPTIME_CONTRACT_VERSION};
    use chrono::{Duration, Utc};
    use sqlx::SqlitePool;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_sqlite_url(test_name: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after UNIX_EPOCH")
            .as_nanos();
        let db_path =
            std::env::temp_dir().join(format!("jetstream-monitor-{test_name}-{nanos}.db"));
        format!("sqlite://{}?mode=rwc", db_path.display())
    }

    #[tokio::test]
    async fn migrates_legacy_uptime_schema_and_returns_window_rows() -> anyhow::Result<()> {
        let database_url = temp_sqlite_url("legacy-hourly-uptime");
        let bootstrap_pool = SqlitePool::connect(&database_url).await?;

        // Simulate an older schema missing newer history columns.
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS hourly_uptime (
                hour TEXT PRIMARY KEY,
                stream_a_seconds INTEGER NOT NULL DEFAULT 0,
                stream_b_seconds INTEGER NOT NULL DEFAULT 0
            )
            "#,
        )
        .execute(&bootstrap_pool)
        .await?;

        drop(bootstrap_pool);

        let storage = Storage::new(&database_url).await?;
        let old_hour = Utc::now() - Duration::hours(30);
        let recent_hour = Utc::now() - Duration::hours(1);

        storage
            .save_hourly_uptime(
                old_hour,
                1800,
                1200,
                1800,
                2400,
                2,
                3,
                50,
                60,
                500,
                450,
                7.5,
                8.2,
                20,
                25,
                INTERVAL_UPTIME_CONTRACT_VERSION,
            )
            .await?;

        storage
            .save_hourly_uptime(
                recent_hour,
                3200,
                3000,
                400,
                600,
                1,
                1,
                45,
                55,
                1200,
                1100,
                4.4,
                5.5,
                15,
                16,
                INTERVAL_UPTIME_CONTRACT_VERSION,
            )
            .await?;

        let rows = storage
            .get_uptime_since(Utc::now() - Duration::hours(24))
            .await?;
        assert_eq!(rows.len(), 1);

        let row = &rows[0];
        assert!(row.hour.contains(' '));
        assert_eq!(row.stream_a_seconds, 3200);
        assert_eq!(row.stream_b_seconds, 3000);
        assert_eq!(row.stream_a_downtime_seconds, 400);
        assert_eq!(row.stream_b_downtime_seconds, 600);
        assert_eq!(row.stream_a_disconnects, 1);
        assert_eq!(row.stream_b_disconnects, 1);
        assert_eq!(row.stream_a_messages, 1200);
        assert_eq!(row.stream_b_messages, 1100);
        assert_eq!(
            row.metrics_contract_version,
            INTERVAL_UPTIME_CONTRACT_VERSION
        );

        Ok(())
    }
}
