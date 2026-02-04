use sqlx::{SqlitePool, sqlite::SqliteConnectOptions, sqlite::SqliteJournalMode};
use std::path::Path;
use chrono::{DateTime, Utc};
use tracing::{debug, error, info};
use crate::models::{
    enriched::EnrichedRecord,
    errors::TurboError,
    TurboResult,
};

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
            "#
        )
        .execute(pool)
        .await?;
        
        debug!("SQLite schema initialized");
        Ok(())
    }
    
    pub async fn store_record(&self, record: &EnrichedRecord) -> TurboResult<i64> {
        let now = Utc::now();
        
        let result = sqlx::query!(
            r#"
            INSERT INTO records (
                at_uri, did, time_us, message, message_metadata,
                created_at, hydrated_at, hydration_time_ms,
                api_calls_count, cache_hit_rate, cache_hits, cache_misses
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
            record.get_at_uri(),
            record.get_did(),
            record.message.time_us,
            serde_json::to_value(&record.message)?,
            serde_json::to_value(&record.hydrated_metadata)?,
            record.processed_at.to_rfc3339(),
            now.to_rfc3339(),
            record.metrics.hydration_time_ms as i64,
            record.metrics.api_calls_count as i64,
            record.metrics.cache_hit_rate,
            record.metrics.cache_hits as i64,
            record.metrics.cache_misses as i64
        )
        .execute(&self.pool)
        .await?;
        
        let id = result.last_insert_rowid();
        debug!("Stored record with ID: {}", id);
        Ok(id)
    }
    
    pub async fn store_batch(&self, records: &[EnrichedRecord]) -> TurboResult<Vec<i64>> {
        if records.is_empty() {
            return Ok(vec![]);
        }
        
        let mut ids = Vec::with_capacity(records.len());
        
        // Begin transaction
        let mut tx = self.pool.begin().await?;
        
        for record in records {
            let id = self.store_record_in_tx(&mut tx, record).await?;
            ids.push(id);
        }
        
        // Commit transaction
        tx.commit().await?;
        
        info!("Stored batch of {} records", records.len());
        Ok(ids)
    }
    
    async fn store_record_in_tx(
        &self, 
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, 
        record: &EnrichedRecord
    ) -> TurboResult<i64> {
        let now = Utc::now();
        
        let result = sqlx::query!(
            r#"
            INSERT INTO records (
                at_uri, did, time_us, message, message_metadata,
                created_at, hydrated_at, hydration_time_ms,
                api_calls_count, cache_hit_rate, cache_hits, cache_misses
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
            record.get_at_uri(),
            record.get_did(),
            record.message.time_us,
            serde_json::to_value(&record.message)?,
            serde_json::to_value(&record.hydrated_metadata)?,
            record.processed_at.to_rfc3339(),
            now.to_rfc3339(),
            record.metrics.hydration_time_ms as i64,
            record.metrics.api_calls_count as i64,
            record.metrics.cache_hit_rate,
            record.metrics.cache_hits as i64,
            record.metrics.cache_misses as i64
        )
        .execute(&mut *tx)
        .await?;
        
        Ok(result.last_insert_rowid())
    }
    
    pub async fn get_record_by_uri(&self, at_uri: &str) -> TurboResult<Option<EnrichedRecord>> {
        let row = sqlx::query!(
            r#"
            SELECT at_uri, did, time_us, message, message_metadata,
                   created_at, hydrated_at, hydration_time_ms,
                   api_calls_count, cache_hit_rate, cache_hits, cache_misses
            FROM records 
            WHERE at_uri = ?
            LIMIT 1
            "#,
            at_uri
        )
        .fetch_optional(&self.pool)
        .await?;
        
        match row {
            Some(row) => {
                let record = self.row_to_record(row)?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }
    
    async fn row_to_record(&self, row: sqlx::sqlite::SqliteRow) -> TurboResult<EnrichedRecord> {
        let message: serde_json::Value = serde_json::from_str(row.get::<String, _>("message"))?;
        let hydrated_metadata: serde_json::Value = serde_json::from_str(row.get::<String, _>("message_metadata"))?;
        
        let message = serde_json::from_value(message)?;
        let hydrated_metadata = serde_json::from_value(hydrated_metadata)?;
        
        Ok(EnrichedRecord {
            message,
            hydrated_metadata,
            processed_at: DateTime::parse_from_rfc3339(row.get("hydrated_at"))?.with_timezone(&Utc),
            metrics: crate::models::enriched::ProcessingMetrics {
                hydration_time_ms: row.get("hydration_time_ms") as u64,
                api_calls_count: row.get("api_calls_count") as u32,
                cache_hit_rate: row.get("cache_hit_rate"),
                cache_hits: row.get("cache_hits") as u32,
                cache_misses: row.get("cache_misses") as u32,
            },
        })
    }
    
    pub async fn count_records(&self) -> TurboResult<i64> {
        let result = sqlx::query!("SELECT COUNT(*) as count FROM records")
            .fetch_one(&self.pool)
            .await?;
        
        Ok(result.count)
    }
    
    pub async fn cleanup_old_records(&self, older_than: DateTime<Utc>) -> TurboResult<u64> {
        let result = sqlx::query!(
            "DELETE FROM records WHERE created_at < ?",
            older_than.to_rfc3339()
        )
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