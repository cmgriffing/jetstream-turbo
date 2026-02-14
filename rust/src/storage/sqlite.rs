use crate::models::{enriched::EnrichedRecord, TurboResult};
use chrono::{DateTime, Utc};
use simd_json::to_string as simd_json_to_string;
use sqlx::{sqlite::SqliteConnectOptions, sqlite::SqliteJournalMode, Row, SqlitePool};
use std::path::Path;
use tracing::{info, trace};

pub struct SQLiteStore {
    pool: SqlitePool,
    db_path: String,
}

impl SQLiteStore {
    pub async fn new<P: AsRef<Path>>(db_path: P) -> TurboResult<Self> {
        let db_path = db_path.as_ref().to_string_lossy().to_string();

        info!("Creating SQLite database at: {}", db_path);

        // Ensure parent directory exists
        if let Some(parent) = Path::new(&db_path).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let connect_options = SqliteConnectOptions::new()
            .filename(&db_path)
            .journal_mode(SqliteJournalMode::Wal)
            .create_if_missing(true);

        let pool = SqlitePool::connect_with(connect_options).await?;

        // Apply performance optimizations
        Self::apply_pragmas(&pool).await?;

        // Initialize schema
        Self::initialize_schema(&pool).await?;

        Ok(Self { pool, db_path })
    }

    async fn initialize_schema(pool: &SqlitePool) -> TurboResult<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                at_uri TEXT CHECK(LENGTH(at_uri) <= 300),
                did TEXT CHECK(LENGTH(did) <= 100),
                time_us INTEGER,
                message TEXT NOT NULL CHECK(json_valid(message)),
                message_metadata TEXT CHECK(json_valid(message_metadata)),
                created_at TEXT NOT NULL,
                hydrated_at TEXT NOT NULL,
                hydration_time_ms INTEGER,
                api_calls_count INTEGER,
                cache_hit_rate REAL,
                cache_hits INTEGER,
                cache_misses INTEGER
            );
            
            CREATE INDEX IF NOT EXISTS idx_records_at_uri ON records(at_uri);
            CREATE INDEX IF NOT EXISTS idx_records_did ON records(did);
            CREATE INDEX IF NOT EXISTS idx_records_time_us ON records(time_us);
            CREATE INDEX IF NOT EXISTS idx_records_created_at ON records(created_at);
            "#,
        )
        .execute(pool)
        .await?;

        trace!("SQLite schema initialized");
        Ok(())
    }

    async fn apply_pragmas(pool: &SqlitePool) -> TurboResult<()> {
        // synchronous = NORMAL: Good performance with WAL mode, still safe
        sqlx::query("PRAGMA synchronous = NORMAL")
            .execute(pool)
            .await?;

        // cache_size = -64000: 64MB page cache (negative = KB units)
        sqlx::query("PRAGMA cache_size = -64000")
            .execute(pool)
            .await?;

        // temp_store = MEMORY: Keep temp tables/indexes in memory
        sqlx::query("PRAGMA temp_store = MEMORY")
            .execute(pool)
            .await?;

        // mmap_size = 268435456: 256MB memory-mapped I/O for faster reads
        sqlx::query("PRAGMA mmap_size = 268435456")
            .execute(pool)
            .await?;

        info!("Applied SQLite performance PRAGMAs");
        Ok(())
    }

    pub async fn store_record(&self, record: &EnrichedRecord) -> TurboResult<i64> {
        let now = Utc::now();

        let message_json = simd_json_to_string(&record.message).unwrap();
        let metadata_json = simd_json_to_string(&record.hydrated_metadata).unwrap();

        let result = sqlx::query(
            r#"
            INSERT INTO records (
                at_uri, did, time_us, message, message_metadata,
                created_at, hydrated_at, hydration_time_ms,
                api_calls_count, cache_hit_rate, cache_hits, cache_misses
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(record.get_at_uri())
        .bind(record.get_did())
        .bind(record.message.time_us.map(|t| t as i64))
        .bind(message_json)
        .bind(metadata_json)
        .bind(record.processed_at.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(record.metrics.hydration_time_ms as i64)
        .bind(record.metrics.api_calls_count as i64)
        .bind(record.metrics.cache_hit_rate)
        .bind(record.metrics.cache_hits as i64)
        .bind(record.metrics.cache_misses as i64)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        trace!("Stored record with ID: {}", id);
        Ok(id)
    }

    pub async fn store_batch(&self, records: &[EnrichedRecord]) -> TurboResult<Vec<i64>> {
        if records.is_empty() {
            return Ok(vec![]);
        }

        let mut tx = self.pool.begin().await?;
        let now = Utc::now();
        let now_str = now.to_rfc3339();

        let insert_sql = r#"
            INSERT INTO records (
                at_uri, did, time_us, message, message_metadata,
                created_at, hydrated_at, hydration_time_ms,
                api_calls_count, cache_hit_rate, cache_hits, cache_misses
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#;

        let mut last_id = 0i64;

        for record in records {
            let message_json = simd_json_to_string(&record.message).unwrap();
            let metadata_json = simd_json_to_string(&record.hydrated_metadata).unwrap();

            let result = sqlx::query(insert_sql)
                .bind(record.get_at_uri())
                .bind(record.get_did())
                .bind(record.message.time_us.map(|t| t as i64))
                .bind(message_json)
                .bind(metadata_json)
                .bind(record.processed_at.to_rfc3339())
                .bind(&now_str)
                .bind(record.metrics.hydration_time_ms as i64)
                .bind(record.metrics.api_calls_count as i64)
                .bind(record.metrics.cache_hit_rate)
                .bind(record.metrics.cache_hits as i64)
                .bind(record.metrics.cache_misses as i64)
                .execute(&mut *tx)
                .await?;

            last_id = result.last_insert_rowid();
        }

        tx.commit().await?;

        let ids: Vec<i64> = (0..records.len())
            .map(|i| last_id - (records.len() - 1 - i) as i64)
            .collect();

        trace!("Stored batch of {} records", records.len());
        Ok(ids)
    }

    pub async fn get_record_by_uri(&self, at_uri: &str) -> TurboResult<Option<EnrichedRecord>> {
        let row = sqlx::query(
            r#"
            SELECT at_uri, did, time_us, message, message_metadata,
                   created_at, hydrated_at, hydration_time_ms,
                   api_calls_count, cache_hit_rate, cache_hits, cache_misses
            FROM records 
            WHERE at_uri = ?
            LIMIT 1
            "#,
        )
        .bind(at_uri)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(row) => {
                let record = self.row_to_record(row).await?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }

    async fn row_to_record(&self, row: sqlx::sqlite::SqliteRow) -> TurboResult<EnrichedRecord> {
        let message_str: String = row.try_get("message")?;
        let metadata_str: String = row.try_get("message_metadata")?;

        let message: serde_json::Value = serde_json::from_str(&message_str)?;
        let hydrated_metadata: serde_json::Value = serde_json::from_str(&metadata_str)?;

        let message = serde_json::from_value(message)?;
        let hydrated_metadata = serde_json::from_value(hydrated_metadata)?;

        let hydrated_at: String = row.try_get("hydrated_at")?;
        let processed_at = DateTime::parse_from_rfc3339(&hydrated_at)
            .map_err(|e| {
                crate::models::errors::TurboError::InvalidMessage(format!("Date parse error: {e}"))
            })?
            .with_timezone(&Utc);

        Ok(EnrichedRecord {
            message,
            hydrated_metadata,
            processed_at,
            metrics: crate::models::enriched::ProcessingMetrics {
                hydration_time_ms: row.try_get::<i64, _>("hydration_time_ms").unwrap_or(0) as u64,
                api_calls_count: row.try_get::<i64, _>("api_calls_count").unwrap_or(0) as u32,
                cache_hit_rate: row.try_get("cache_hit_rate").unwrap_or(0.0),
                cache_hits: row.try_get::<i64, _>("cache_hits").unwrap_or(0) as u32,
                cache_misses: row.try_get::<i64, _>("cache_misses").unwrap_or(0) as u32,
            },
        })
    }

    pub async fn count_records(&self) -> TurboResult<i64> {
        let result = sqlx::query("SELECT COUNT(*) as count FROM records")
            .fetch_one(&self.pool)
            .await?;

        let count: i64 = result.try_get("count")?;
        Ok(count)
    }

    pub async fn cleanup_old_records(&self, older_than: DateTime<Utc>) -> TurboResult<u64> {
        let older_than_str = older_than.to_rfc3339();
        let result = sqlx::query("DELETE FROM records WHERE created_at < ?")
            .bind(&older_than_str)
            .execute(&self.pool)
            .await?;

        let deleted = result.rows_affected();
        info!("Cleaned up {} old records", deleted);
        Ok(deleted)
    }

    pub async fn get_db_path(&self) -> &str {
        &self.db_path
    }

    pub async fn close(&self) -> TurboResult<()> {
        self.pool.close().await;
        info!("SQLite connection pool closed");
        Ok(())
    }
}
