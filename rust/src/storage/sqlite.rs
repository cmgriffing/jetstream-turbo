use crate::models::{
    enriched::EnrichedRecord,
    recovery::{IngestionCheckpoint, SourceCursor, SourceEventId},
    TurboError, TurboResult,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use simd_json::to_string as simd_json_to_string;
use sqlx::{
    sqlite::SqliteConnectOptions, sqlite::SqliteJournalMode, sqlite::SqlitePoolOptions, Row,
    SqliteConnection, SqlitePool,
};
use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;
use tokio::time::{sleep, Duration};
use tracing::{error, info, instrument, trace, warn};

#[derive(Debug, Clone, Serialize)]
pub struct CleanupResult {
    pub records_deleted: u64,
    pub new_size_bytes: i64,
    pub vacuum_pending: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SQLiteStateSnapshot {
    pub db_size_bytes: i64,
    pub wal_size_bytes: Option<i64>,
    pub page_count: i64,
    pub page_size_bytes: i64,
    pub freelist_count: i64,
    pub cache_size_pages: i64,
    pub mmap_size_bytes: i64,
    pub journal_mode: String,
    pub journal_size_limit_bytes: i64,
    pub partial_records: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct SQLitePragmaConfig {
    pub cache_size_kib: u32,
    pub mmap_size_mb: u64,
    pub journal_size_limit_mb: u64,
}

pub trait RecordStore {
    fn store_batch(
        &self,
        records: &[EnrichedRecord],
    ) -> impl std::future::Future<Output = TurboResult<Vec<i64>>> + Send;

    fn completed_source_event_ids(
        &self,
        _source_event_ids: &[SourceEventId],
    ) -> impl std::future::Future<Output = TurboResult<HashSet<SourceEventId>>> + Send {
        async { Ok(HashSet::new()) }
    }
}

pub struct SQLiteStore {
    pool: SqlitePool,
    db_path: String,
}

impl SQLiteStore {
    pub async fn new<P: AsRef<Path>>(
        db_path: P,
        pragma_config: SQLitePragmaConfig,
    ) -> TurboResult<Self> {
        let db_path_str = db_path.as_ref().to_string_lossy().to_string();

        info!("Creating SQLite database at: {}", db_path_str);

        // Ensure parent directory exists (skip for in-memory databases)
        if db_path_str != ":memory:" {
            if let Some(parent) = Path::new(&db_path_str).parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }

        let mut connect_options = SqliteConnectOptions::new()
            .filename(&db_path_str)
            .create_if_missing(true);

        // Skip WAL mode for in-memory databases
        if db_path_str != ":memory:" {
            connect_options = connect_options.journal_mode(SqliteJournalMode::Wal);
        }

        let pool = SqlitePoolOptions::new()
            .after_connect({
                let db_path = db_path_str.clone();
                move |conn, _meta| {
                    let db_path = db_path.clone();
                    Box::pin(async move {
                        Self::apply_pragmas(conn, pragma_config, &db_path).await?;
                        Ok(())
                    })
                }
            })
            .connect_with(connect_options)
            .await?;

        // Initialize schema
        Self::initialize_schema(&pool).await?;

        Ok(Self {
            pool,
            db_path: db_path_str,
        })
    }

    async fn initialize_schema(pool: &SqlitePool) -> TurboResult<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                at_uri TEXT CHECK(LENGTH(at_uri) <= 300),
                did TEXT CHECK(LENGTH(did) <= 100),
                time_us INTEGER,
                source_event_id TEXT,
                message TEXT NOT NULL CHECK(json_valid(message)),
                message_metadata TEXT CHECK(json_valid(message_metadata)),
                created_at TEXT NOT NULL,
                hydrated_at TEXT NOT NULL,
                hydration_time_ms INTEGER,
                api_calls_count INTEGER,
                cache_hit_rate REAL,
                cache_hits INTEGER,
                cache_misses INTEGER,
                hydration_quality TEXT NOT NULL DEFAULT 'unknown'
                    CHECK(hydration_quality IN ('unknown', 'complete', 'partial'))
            );
            
            CREATE INDEX IF NOT EXISTS idx_records_at_uri ON records(at_uri);
            CREATE INDEX IF NOT EXISTS idx_records_did ON records(did);
            CREATE INDEX IF NOT EXISTS idx_records_time_us ON records(time_us);
            CREATE INDEX IF NOT EXISTS idx_records_created_at ON records(created_at);
            CREATE TABLE IF NOT EXISTS ingestion_checkpoint (
                singleton_id INTEGER PRIMARY KEY CHECK(singleton_id = 1),
                ingress_ordinal INTEGER NOT NULL,
                time_us INTEGER NOT NULL,
                source_seq INTEGER,
                source_event_id TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            "#,
        )
        .execute(pool)
        .await?;

        let columns = sqlx::query("PRAGMA table_info(records)")
            .fetch_all(pool)
            .await?;
        let has_source_event_id = columns.iter().any(|row| {
            row.try_get::<String, _>("name")
                .is_ok_and(|name| name == "source_event_id")
        });
        if !has_source_event_id {
            sqlx::query("ALTER TABLE records ADD COLUMN source_event_id TEXT")
                .execute(pool)
                .await?;
        }
        let has_hydration_quality = columns.iter().any(|row| {
            row.try_get::<String, _>("name")
                .is_ok_and(|name| name == "hydration_quality")
        });
        if !has_hydration_quality {
            sqlx::query(
                "ALTER TABLE records ADD COLUMN hydration_quality TEXT NOT NULL DEFAULT 'unknown'",
            )
            .execute(pool)
            .await?;
        }
        sqlx::query("UPDATE records SET hydration_quality = 'unknown' WHERE hydration_quality IS NULL OR hydration_quality NOT IN ('unknown', 'complete', 'partial')")
            .execute(pool)
            .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_records_hydration_quality ON records(hydration_quality)",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_records_source_event_id ON records(source_event_id) WHERE source_event_id IS NOT NULL",
        )
        .execute(pool)
        .await?;

        trace!("SQLite schema initialized");
        Ok(())
    }

    async fn apply_pragmas(
        conn: &mut SqliteConnection,
        pragma_config: SQLitePragmaConfig,
        db_path: &str,
    ) -> Result<(), sqlx::Error> {
        // synchronous = NORMAL: Good performance with WAL mode, still safe
        sqlx::query("PRAGMA synchronous = NORMAL")
            .execute(&mut *conn)
            .await?;

        let cache_size_pragma = -(pragma_config.cache_size_kib as i64);
        // cache_size uses negative values to mean kibibytes.
        sqlx::query(&format!("PRAGMA cache_size = {cache_size_pragma}"))
            .execute(&mut *conn)
            .await?;

        // temp_store = MEMORY: Keep temp tables/indexes in memory
        sqlx::query("PRAGMA temp_store = MEMORY")
            .execute(&mut *conn)
            .await?;

        let mmap_size_bytes = pragma_config.mmap_size_mb.saturating_mul(1024 * 1024);
        // mmap_size for faster reads (skip for in-memory)
        // In-memory databases don't benefit from mmap
        let _ = sqlx::query(&format!("PRAGMA mmap_size = {mmap_size_bytes}"))
            .execute(&mut *conn)
            .await;

        // Limit WAL size to prevent unbounded growth.
        let journal_size_limit_bytes = pragma_config
            .journal_size_limit_mb
            .saturating_mul(1024 * 1024);
        sqlx::query(&format!(
            "PRAGMA journal_size_limit = {journal_size_limit_bytes}"
        ))
        .execute(&mut *conn)
        .await?;

        let (effective_mmap_size_bytes,): (i64,) = sqlx::query_as("PRAGMA mmap_size")
            .fetch_one(&mut *conn)
            .await?;
        let (effective_journal_size_limit_bytes,): (i64,) =
            sqlx::query_as("PRAGMA journal_size_limit")
                .fetch_one(&mut *conn)
                .await?;

        info!(
            "Applied SQLite PRAGMAs to {db_path}: cache_size={}KiB, mmap_size={}MB (effective {} bytes), journal_size_limit={}MB (effective {} bytes)",
            pragma_config.cache_size_kib,
            pragma_config.mmap_size_mb,
            effective_mmap_size_bytes,
            pragma_config.journal_size_limit_mb,
            effective_journal_size_limit_bytes
        );

        if pragma_config.mmap_size_mb > 0 && effective_mmap_size_bytes == 0 {
            warn!(
                "SQLite mmap_size requested for {db_path} but remains disabled on this connection"
            );
        }

        if effective_journal_size_limit_bytes != journal_size_limit_bytes as i64 {
            warn!(
                "SQLite journal_size_limit requested for {db_path} but effective value is {} bytes instead of {} bytes",
                effective_journal_size_limit_bytes,
                journal_size_limit_bytes
            );
        }

        Ok(())
    }

    pub async fn store_record(&self, record: &EnrichedRecord) -> TurboResult<i64> {
        let now = Utc::now();

        let message_json = simd_json_to_string(&record.message).unwrap();
        let metadata_json = simd_json_to_string(&record.hydrated_metadata).unwrap();

        let id: i64 = sqlx::query_scalar(
            r#"
            INSERT INTO records (
                at_uri, did, time_us, source_event_id, message, message_metadata,
                created_at, hydrated_at, hydration_time_ms,
                api_calls_count, cache_hit_rate, cache_hits, cache_misses,
                hydration_quality
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT DO UPDATE SET
                source_event_id = excluded.source_event_id
            RETURNING id
            "#,
        )
        .bind(record.get_at_uri())
        .bind(record.get_did())
        .bind(record.message.time_us.map(|t| t as i64))
        .bind(record.source_event_id().to_string())
        .bind(message_json)
        .bind(metadata_json)
        .bind(record.processed_at.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(record.metrics.hydration_time_ms as i64)
        .bind(record.metrics.api_calls_count as i64)
        .bind(record.metrics.cache_hit_rate)
        .bind(record.metrics.cache_hits as i64)
        .bind(record.metrics.cache_misses as i64)
        .bind(record.hydrated_metadata.hydration_quality.as_str())
        .fetch_one(&self.pool)
        .await?;

        Ok(id)
    }

    pub async fn get_record_by_uri(&self, at_uri: &str) -> TurboResult<Option<EnrichedRecord>> {
        let row = sqlx::query(
            r#"
            SELECT at_uri, did, time_us, message, message_metadata,
                   created_at, hydrated_at, hydration_time_ms,
                   api_calls_count, cache_hit_rate, cache_hits, cache_misses,
                   hydration_quality
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

    pub async fn has_completed_source_event(
        &self,
        source_event_id: &SourceEventId,
    ) -> TurboResult<bool> {
        let exists: i64 =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM records WHERE source_event_id = ?)")
                .bind(source_event_id.as_str())
                .fetch_one(&self.pool)
                .await?;
        Ok(exists != 0)
    }

    async fn row_to_record(&self, row: sqlx::sqlite::SqliteRow) -> TurboResult<EnrichedRecord> {
        let message_str: String = row.try_get("message")?;
        let metadata_str: String = row.try_get("message_metadata")?;

        let message: serde_json::Value = serde_json::from_str(&message_str)?;
        let hydrated_metadata: serde_json::Value = serde_json::from_str(&metadata_str)?;

        let mut message: crate::models::jetstream::JetstreamMessage =
            serde_json::from_value(message)?;
        message.populate_record_from_wire(&message_str);
        let mut hydrated_metadata: crate::models::enriched::HydratedMetadata =
            serde_json::from_value(hydrated_metadata)?;
        if let Ok(quality) = row.try_get::<String, _>("hydration_quality") {
            hydrated_metadata.hydration_quality =
                crate::models::enriched::HydrationQuality::from_storage(&quality);
        }

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

    /// Selects a bounded set of partial records for a future repair worker.
    /// This read-only query intentionally does not touch the ingestion checkpoint.
    pub async fn select_partial_records(&self, limit: usize) -> TurboResult<Vec<EnrichedRecord>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"
            SELECT at_uri, did, time_us, message, message_metadata,
                   created_at, hydrated_at, hydration_time_ms,
                   api_calls_count, cache_hit_rate, cache_hits, cache_misses,
                   hydration_quality
            FROM records
            WHERE hydration_quality = 'partial'
            ORDER BY id ASC
            LIMIT ?
            "#,
        )
        .bind(limit.min(1_000) as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            records.push(self.row_to_record(row).await?);
        }
        Ok(records)
    }

    /// Loads the singleton durable ingestion checkpoint, if one has been committed.
    pub async fn load_ingestion_checkpoint(&self) -> TurboResult<Option<IngestionCheckpoint>> {
        let row = sqlx::query(
            r#"
            SELECT ingress_ordinal, time_us, source_seq, source_event_id, updated_at
            FROM ingestion_checkpoint
            WHERE singleton_id = 1
            "#,
        )
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| {
            let ingress_ordinal = u64::try_from(row.try_get::<i64, _>("ingress_ordinal")?)
                .map_err(|_| {
                    TurboError::InvalidMessage("negative checkpoint ordinal".to_string())
                })?;
            let time_us = u64::try_from(row.try_get::<i64, _>("time_us")?).map_err(|_| {
                TurboError::InvalidMessage("negative checkpoint time_us".to_string())
            })?;
            let source_seq = row
                .try_get::<Option<i64>, _>("source_seq")?
                .map(u64::try_from)
                .transpose()
                .map_err(|_| {
                    TurboError::InvalidMessage("negative checkpoint source_seq".to_string())
                })?;
            let updated_at = DateTime::parse_from_rfc3339(&row.try_get::<String, _>("updated_at")?)
                .map_err(|error| {
                    TurboError::InvalidMessage(format!("invalid checkpoint updated_at: {error}"))
                })?
                .with_timezone(&Utc);

            Ok(IngestionCheckpoint {
                ingress_ordinal,
                cursor: SourceCursor {
                    time_us,
                    source_seq,
                    source_event_id: SourceEventId::from(
                        row.try_get::<String, _>("source_event_id")?,
                    ),
                },
                updated_at,
            })
        })
        .transpose()
    }

    /// Atomically advances the singleton checkpoint when `checkpoint` is newer.
    pub async fn advance_ingestion_checkpoint(
        &self,
        checkpoint: &IngestionCheckpoint,
    ) -> TurboResult<bool> {
        let ingress_ordinal = i64::try_from(checkpoint.ingress_ordinal).map_err(|_| {
            TurboError::InvalidMessage("checkpoint ordinal exceeds SQLite range".to_string())
        })?;
        let time_us = i64::try_from(checkpoint.cursor.time_us).map_err(|_| {
            TurboError::InvalidMessage("checkpoint time_us exceeds SQLite range".to_string())
        })?;
        let source_seq = checkpoint
            .cursor
            .source_seq
            .map(i64::try_from)
            .transpose()
            .map_err(|_| {
                TurboError::InvalidMessage("checkpoint source_seq exceeds SQLite range".to_string())
            })?;

        let result = sqlx::query(
            r#"
            INSERT INTO ingestion_checkpoint (
                singleton_id, ingress_ordinal, time_us, source_seq, source_event_id, updated_at
            ) VALUES (1, ?, ?, ?, ?, ?)
            ON CONFLICT(singleton_id) DO UPDATE SET
                ingress_ordinal = excluded.ingress_ordinal,
                time_us = excluded.time_us,
                source_seq = excluded.source_seq,
                source_event_id = excluded.source_event_id,
                updated_at = excluded.updated_at
            WHERE excluded.ingress_ordinal > ingestion_checkpoint.ingress_ordinal
            "#,
        )
        .bind(ingress_ordinal)
        .bind(time_us)
        .bind(source_seq)
        .bind(checkpoint.cursor.source_event_id.as_str())
        .bind(checkpoint.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() == 1)
    }

    pub async fn cleanup_old_records(
        &self,
        older_than: DateTime<Utc>,
        chunk_size: u32,
        chunk_delay_ms: u64,
    ) -> TurboResult<u64> {
        let older_than_str = older_than.to_rfc3339();
        let mut total_deleted = 0u64;

        loop {
            let result = sqlx::query(
                "DELETE FROM records WHERE rowid IN (SELECT rowid FROM records WHERE created_at < ? LIMIT ?)"
            )
            .bind(&older_than_str)
            .bind(chunk_size)
            .execute(&self.pool)
            .await?;

            let deleted = result.rows_affected();
            if deleted == 0 {
                break;
            }

            total_deleted += deleted;

            if deleted as u32 == chunk_size {
                sleep(Duration::from_millis(chunk_delay_ms)).await;
            }
        }

        info!("Cleaned up {} old records", total_deleted);
        Ok(total_deleted)
    }

    pub async fn get_db_size(&self) -> TurboResult<i64> {
        let row: (i64,) = sqlx::query_as(
            "SELECT (page_count * page_size) as size FROM pragma_page_count(), pragma_page_size()",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    pub async fn get_state_snapshot(&self) -> TurboResult<SQLiteStateSnapshot> {
        let db_size_bytes = self.get_db_size().await?;
        let wal_size_bytes = self.get_wal_size_bytes().await?;

        let (page_count,): (i64,) = sqlx::query_as("PRAGMA page_count")
            .fetch_one(&self.pool)
            .await?;
        let (page_size_bytes,): (i64,) = sqlx::query_as("PRAGMA page_size")
            .fetch_one(&self.pool)
            .await?;
        let (freelist_count,): (i64,) = sqlx::query_as("PRAGMA freelist_count")
            .fetch_one(&self.pool)
            .await?;
        let (cache_size_pages,): (i64,) = sqlx::query_as("PRAGMA cache_size")
            .fetch_one(&self.pool)
            .await?;
        let (mmap_size_bytes,): (i64,) = sqlx::query_as("PRAGMA mmap_size")
            .fetch_one(&self.pool)
            .await?;
        let (journal_mode,): (String,) = sqlx::query_as("PRAGMA journal_mode")
            .fetch_one(&self.pool)
            .await?;
        let (journal_size_limit_bytes,): (i64,) = sqlx::query_as("PRAGMA journal_size_limit")
            .fetch_one(&self.pool)
            .await?;
        let partial_records: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM records WHERE hydration_quality = 'partial'")
                .fetch_one(&self.pool)
                .await?;

        Ok(SQLiteStateSnapshot {
            db_size_bytes,
            wal_size_bytes,
            page_count,
            page_size_bytes,
            freelist_count,
            cache_size_pages,
            mmap_size_bytes,
            journal_mode,
            journal_size_limit_bytes,
            partial_records,
        })
    }

    async fn get_wal_size_bytes(&self) -> TurboResult<Option<i64>> {
        if self.db_path == ":memory:" {
            return Ok(None);
        }

        let wal_path = format!("{}-wal", self.db_path);
        match tokio::fs::metadata(wal_path).await {
            Ok(metadata) => Ok(Some(metadata.len() as i64)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Some(0)),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn cleanup_with_vacuum(
        &self,
        retention_days: u32,
        max_size_bytes: i64,
        vacuum_min_bytes_freed: u64,
        vacuum_min_percent_freed: f64,
        cleanup_chunk_size: u32,
        cleanup_chunk_delay_ms: u64,
    ) -> TurboResult<CleanupResult> {
        let initial_size = self.get_db_size().await?;
        let mut current_retention = retention_days;
        let mut total_deleted: u64 = 0;
        let max_iterations = 3;

        for iteration in 0..max_iterations {
            let cutoff = Utc::now() - chrono::Duration::days(current_retention as i64);
            let deleted = self
                .cleanup_old_records(cutoff, cleanup_chunk_size, cleanup_chunk_delay_ms)
                .await?;
            total_deleted += deleted;

            let current_size = self.get_db_size().await?;

            if current_size <= max_size_bytes {
                break;
            }

            info!(
                "Iteration {}: DB still {}MB over limit, reducing retention from {} to {} days",
                iteration + 1,
                current_size / (1024 * 1024),
                current_retention,
                (current_retention / 2).max(1)
            );

            current_retention = (current_retention / 2).max(1);

            if iteration < max_iterations - 1 {
                sleep(Duration::from_secs(2)).await;
            }
        }

        let post_delete_size = self.get_db_size().await?;
        let bytes_freed = initial_size.saturating_sub(post_delete_size);
        let percent_freed = if initial_size > 0 {
            (bytes_freed as f64 / initial_size as f64) * 100.0
        } else {
            0.0
        };

        let should_vacuum = bytes_freed as i64 >= vacuum_min_bytes_freed as i64
            || percent_freed >= vacuum_min_percent_freed;

        let mut vacuum_pending = false;

        if should_vacuum {
            let pool = self.pool.clone();
            let freed_mb = bytes_freed / (1024 * 1024);
            let freed_percent = percent_freed as u64;
            tokio::spawn(async move {
                info!(
                    "Starting background VACUUM (freed {}MB, {}%)",
                    freed_mb, freed_percent
                );
                match sqlx::query("VACUUM").execute(&pool).await {
                    Ok(_) => info!("Background VACUUM completed"),
                    Err(e) => error!("Background VACUUM failed: {}", e),
                }
            });

            sleep(Duration::from_millis(500)).await;
            vacuum_pending = true;
        } else {
            info!(
                "Skipping VACUUM: freed {}MB ({}%), below threshold ({}MB, {}%)",
                bytes_freed / (1024 * 1024),
                percent_freed as u64,
                vacuum_min_bytes_freed / (1024 * 1024),
                vacuum_min_percent_freed as u64
            );
        }

        Ok(CleanupResult {
            records_deleted: total_deleted,
            new_size_bytes: post_delete_size,
            vacuum_pending,
        })
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

impl RecordStore for SQLiteStore {
    async fn completed_source_event_ids(
        &self,
        source_event_ids: &[SourceEventId],
    ) -> TurboResult<HashSet<SourceEventId>> {
        let mut completed = HashSet::new();
        for source_event_id in source_event_ids {
            if self.has_completed_source_event(source_event_id).await? {
                completed.insert(source_event_id.clone());
            }
        }
        Ok(completed)
    }

    #[instrument(
        name = "sqlite_store_batch",
        skip(self, records),
        fields(count, duration_ms)
    )]
    async fn store_batch(&self, records: &[EnrichedRecord]) -> TurboResult<Vec<i64>> {
        let start = Instant::now();

        if records.is_empty() {
            return Ok(vec![]);
        }

        let count = records.len();
        tracing::Span::current().record("count", count);

        let now = Utc::now();
        let now_str = now.to_rfc3339();

        const MAX_PARAMS: usize = 999;
        const COLUMNS: usize = 14;
        const MAX_ROWS_PER_INSERT: usize = MAX_PARAMS / COLUMNS;

        static SINGLE_ROW_PLACEHOLDER: &str = "(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

        let mut all_ids = Vec::with_capacity(count);

        for chunk in records.chunks(MAX_ROWS_PER_INSERT) {
            let mut tx = self.pool.begin().await?;

            let placeholders: String = std::iter::repeat_n(SINGLE_ROW_PLACEHOLDER, chunk.len())
                .collect::<Vec<_>>()
                .join(", ");

            let insert_sql = format!(
                r#"INSERT INTO records (
                    at_uri, did, time_us, source_event_id, message, message_metadata,
                    created_at, hydrated_at, hydration_time_ms,
                    api_calls_count, cache_hit_rate, cache_hits, cache_misses,
                    hydration_quality
                ) VALUES {placeholders}
                ON CONFLICT DO UPDATE SET
                    source_event_id = excluded.source_event_id
                RETURNING id"#
            );

            let mut query = sqlx::query(&insert_sql);

            for record in chunk {
                query = query
                    .bind(record.get_at_uri())
                    .bind(record.get_did())
                    .bind(record.message.time_us.map(|t| t as i64))
                    .bind(record.source_event_id().to_string())
                    .bind(simd_json_to_string(&record.message).unwrap())
                    .bind(simd_json_to_string(&record.hydrated_metadata).unwrap())
                    .bind(record.processed_at.to_rfc3339())
                    .bind(&now_str)
                    .bind(record.metrics.hydration_time_ms as i64)
                    .bind(record.metrics.api_calls_count as i64)
                    .bind(record.metrics.cache_hit_rate)
                    .bind(record.metrics.cache_hits as i64)
                    .bind(record.metrics.cache_misses as i64)
                    .bind(record.hydrated_metadata.hydration_quality.as_str());
            }

            let rows = query.fetch_all(&mut *tx).await?;
            tx.commit().await?;
            all_ids.extend(
                rows.into_iter()
                    .map(|row| row.try_get::<i64, _>("id"))
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }

        let duration = start.elapsed().as_millis() as u64;
        tracing::Span::current().record("duration_ms", duration);
        trace!("Stored batch of {} records", count);
        Ok(all_ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::enriched::HydrationQuality;
    use crate::models::jetstream::{CommitData, JetstreamMessage, MessageKind, OperationType};
    use chrono::{Duration, Utc};

    async fn create_test_db() -> SQLiteStore {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("test_sqlite_{}.db", uuid::Uuid::new_v4()));
        let db_path_str = db_path.to_string_lossy().to_string();
        SQLiteStore::new(
            &db_path_str,
            SQLitePragmaConfig {
                cache_size_kib: 64 * 1024,
                mmap_size_mb: 256,
                journal_size_limit_mb: 512,
            },
        )
        .await
        .unwrap()
    }

    fn test_checkpoint(ordinal: u64, time_us: u64) -> IngestionCheckpoint {
        IngestionCheckpoint {
            ingress_ordinal: ordinal,
            cursor: SourceCursor {
                time_us,
                source_seq: Some(ordinal * 10),
                source_event_id: SourceEventId::from(format!("event-{ordinal}")),
            },
            updated_at: Utc::now(),
        }
    }

    fn test_record(seq: u64) -> EnrichedRecord {
        EnrichedRecord::new_with_timestamp(
            JetstreamMessage {
                did: "did:plc:stored".to_string(),
                time_us: Some(1_000_000 + seq),
                seq: Some(seq),
                kind: MessageKind::Commit,
                commit: Some(CommitData {
                    rev: Some("rev-1".to_string()),
                    operation_type: OperationType::Create,
                    collection: Some("app.bsky.feed.post".to_string()),
                    rkey: Some(format!("post-{seq}")),
                    record: None,
                    cid: Some(format!("cid-{seq}")),
                }),
                raw_json: None,
            },
            Utc::now(),
        )
    }

    #[tokio::test]
    async fn test_get_db_size() {
        let store = create_test_db().await;

        let size = store.get_db_size().await.unwrap();
        assert!(size > 0, "Database should have some initial size");

        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn ingestion_checkpoint_schema_is_added_to_existing_database() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("existing.db");
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(options).await.unwrap();
        sqlx::query("CREATE TABLE legacy_data (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let store = SQLiteStore::new(
            &db_path,
            SQLitePragmaConfig {
                cache_size_kib: 64 * 1024,
                mmap_size_mb: 256,
                journal_size_limit_mb: 512,
            },
        )
        .await
        .unwrap();

        assert_eq!(store.load_ingestion_checkpoint().await.unwrap(), None);
    }

    #[tokio::test]
    async fn source_event_identity_column_is_added_to_existing_records_table() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("records-migration.db");
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(options).await.unwrap();
        sqlx::query(
            r#"
            CREATE TABLE records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                at_uri TEXT,
                did TEXT,
                time_us INTEGER,
                message TEXT NOT NULL,
                message_metadata TEXT,
                created_at TEXT NOT NULL,
                hydrated_at TEXT NOT NULL,
                hydration_time_ms INTEGER,
                api_calls_count INTEGER,
                cache_hit_rate REAL,
                cache_hits INTEGER,
                cache_misses INTEGER
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"INSERT INTO records (
                at_uri, did, time_us, message, message_metadata, created_at, hydrated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind("at://did:plc:legacy/app.bsky.feed.post/one")
        .bind("did:plc:legacy")
        .bind(1_i64)
        .bind(r#"{"did":"did:plc:legacy","kind":"identity"}"#)
        .bind("{}")
        .bind("2026-01-01T00:00:00Z")
        .bind("2026-01-01T00:00:00Z")
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let store = SQLiteStore::new(
            &db_path,
            SQLitePragmaConfig {
                cache_size_kib: 64 * 1024,
                mmap_size_mb: 256,
                journal_size_limit_mb: 512,
            },
        )
        .await
        .unwrap();
        let columns = sqlx::query("PRAGMA table_info(records)")
            .fetch_all(&store.pool)
            .await
            .unwrap();

        assert!(columns.iter().any(|row| {
            row.try_get::<String, _>("name")
                .is_ok_and(|name| name == "source_event_id")
        }));
        assert!(columns.iter().any(|row| {
            row.try_get::<String, _>("name")
                .is_ok_and(|name| name == "hydration_quality")
        }));
        let legacy_quality: String =
            sqlx::query_scalar("SELECT hydration_quality FROM records LIMIT 1")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(legacy_quality, "unknown");
        let indexes = sqlx::query("PRAGMA index_list(records)")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        assert!(indexes.iter().any(|row| {
            row.try_get::<String, _>("name")
                .is_ok_and(|name| name == "idx_records_hydration_quality")
        }));
    }

    #[tokio::test]
    async fn hydration_quality_round_trips_and_partial_selection_is_bounded() {
        let store = create_test_db().await;
        let mut complete = test_record(1);
        complete.hydrated_metadata.hydration_quality = HydrationQuality::Complete;
        let mut partial_one = test_record(2);
        partial_one.hydrated_metadata.hydration_quality = HydrationQuality::Partial;
        let mut partial_two = test_record(3);
        partial_two.hydrated_metadata.hydration_quality = HydrationQuality::Partial;

        store
            .store_batch(&[complete.clone(), partial_one.clone(), partial_two])
            .await
            .unwrap();

        let loaded = store
            .get_record_by_uri(&complete.get_at_uri().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded.hydrated_metadata.hydration_quality,
            HydrationQuality::Complete
        );

        let selected = store.select_partial_records(1).await.unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(
            selected[0].hydrated_metadata.hydration_quality,
            HydrationQuality::Partial
        );
        assert_eq!(store.select_partial_records(0).await.unwrap().len(), 0);
        assert_eq!(store.load_ingestion_checkpoint().await.unwrap(), None);
    }

    #[tokio::test]
    async fn source_event_identity_recognizes_completed_replay() {
        let store = create_test_db().await;
        let record = test_record(1);
        let source_event_id = record.source_event_id();
        let first_id = store.store_record(&record).await.unwrap();

        assert!(store
            .has_completed_source_event(&source_event_id)
            .await
            .unwrap());
        assert_eq!(store.store_record(&record).await.unwrap(), first_id);
    }

    #[tokio::test]
    async fn ingestion_checkpoint_advances_monotonically() {
        let store = create_test_db().await;
        let newer = test_checkpoint(2, 2_000);

        assert!(store.advance_ingestion_checkpoint(&newer).await.unwrap());
        assert!(!store
            .advance_ingestion_checkpoint(&test_checkpoint(1, 1_000))
            .await
            .unwrap());
        assert_eq!(
            store.load_ingestion_checkpoint().await.unwrap(),
            Some(newer)
        );
    }

    #[tokio::test]
    async fn ingestion_checkpoint_survives_store_restart() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("restart.db");
        let pragma_config = SQLitePragmaConfig {
            cache_size_kib: 64 * 1024,
            mmap_size_mb: 256,
            journal_size_limit_mb: 512,
        };
        let checkpoint = test_checkpoint(7, 7_000);
        let store = SQLiteStore::new(&db_path, pragma_config).await.unwrap();
        store
            .advance_ingestion_checkpoint(&checkpoint)
            .await
            .unwrap();
        store.close().await.unwrap();

        let reopened = SQLiteStore::new(&db_path, pragma_config).await.unwrap();

        assert_eq!(
            reopened.load_ingestion_checkpoint().await.unwrap(),
            Some(checkpoint)
        );
    }

    #[tokio::test]
    async fn test_get_state_snapshot() {
        let store = create_test_db().await;

        let snapshot = store.get_state_snapshot().await.unwrap();
        assert!(snapshot.db_size_bytes > 0);
        assert!(snapshot.page_count > 0);
        assert!(snapshot.page_size_bytes > 0);
        assert!(!snapshot.journal_mode.is_empty());
        assert!(snapshot.wal_size_bytes.is_some());
        assert!(
            snapshot.cache_size_pages < 0,
            "cache_size pragma should remain in kibibyte mode"
        );
        assert!(
            snapshot.mmap_size_bytes == (256 * 1024 * 1024) as i64
                || snapshot.mmap_size_bytes == 0,
            "mmap_size should be configured when supported, or remain disabled in environments where SQLite declines it"
        );
        assert!(
            snapshot.journal_size_limit_bytes == (512 * 1024 * 1024) as i64
                || snapshot.journal_size_limit_bytes == -1,
            "journal_size_limit should be configured or report SQLite's unlimited sentinel"
        );

        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_connection_scoped_pragmas_are_applied_to_each_pool_connection() {
        let store = create_test_db().await;

        let mut conn1 = store.pool.acquire().await.unwrap();
        let mut conn2 = store.pool.acquire().await.unwrap();

        let (cache_size_1,): (i64,) = sqlx::query_as("PRAGMA cache_size")
            .fetch_one(&mut *conn1)
            .await
            .unwrap();
        let (cache_size_2,): (i64,) = sqlx::query_as("PRAGMA cache_size")
            .fetch_one(&mut *conn2)
            .await
            .unwrap();

        assert_eq!(cache_size_1, -(64 * 1024) as i64);
        assert_eq!(cache_size_2, -(64 * 1024) as i64);

        drop(conn2);
        drop(conn1);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_cleanup_old_records_empty_db() {
        let store = create_test_db().await;

        let cutoff = Utc::now() - Duration::days(7);
        let deleted = store.cleanup_old_records(cutoff, 1000, 50).await.unwrap();

        assert_eq!(deleted, 0, "Should delete nothing from empty DB");

        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_cleanup_old_records_with_data() {
        let store = create_test_db().await;

        let now = Utc::now();
        let now_str = now.to_rfc3339();

        let old_time = now - Duration::days(10);
        let old_time_str = old_time.to_rfc3339();

        sqlx::query(
            r#"INSERT INTO records (at_uri, did, time_us, message, message_metadata, created_at, hydrated_at, hydration_time_ms, api_calls_count, cache_hit_rate, cache_hits, cache_misses)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#
        )
        .bind("at://old.bsky.social/app.bsky.feed.post/1")
        .bind("did:plc:old")
        .bind(1000i64)
        .bind(r#"{"foo":"bar"}"#)
        .bind(r#"{}"#)
        .bind(&old_time_str)
        .bind(&now_str)
        .bind(100i64)
        .bind(1i64)
        .bind(0.5)
        .bind(10i64)
        .bind(10i64)
        .execute(&store.pool)
        .await
        .unwrap();

        sqlx::query(
            r#"INSERT INTO records (at_uri, did, time_us, message, message_metadata, created_at, hydrated_at, hydration_time_ms, api_calls_count, cache_hit_rate, cache_hits, cache_misses)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#
        )
        .bind("at://new.bsky.social/app.bsky.feed.post/2")
        .bind("did:plc:new")
        .bind(2000i64)
        .bind(r#"{"foo":"bar"}"#)
        .bind(r#"{}"#)
        .bind(&now_str)
        .bind(&now_str)
        .bind(100i64)
        .bind(1i64)
        .bind(0.5)
        .bind(10i64)
        .bind(10i64)
        .execute(&store.pool)
        .await
        .unwrap();

        let cutoff = now - Duration::days(7);
        let deleted = store.cleanup_old_records(cutoff, 1000, 50).await.unwrap();

        assert_eq!(deleted, 1, "Should delete 1 old record");

        let count = store.count_records().await.unwrap();
        assert_eq!(count, 1, "Should have 1 record remaining");

        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_cleanup_with_vacuum_size_based() {
        let store = create_test_db().await;

        let now = Utc::now();
        let now_str = now.to_rfc3339();

        for i in 0..5 {
            let old_time = now - Duration::days(10);
            let old_time_str = old_time.to_rfc3339();

            sqlx::query(
                r#"INSERT INTO records (at_uri, did, time_us, message, message_metadata, created_at, hydrated_at, hydration_time_ms, api_calls_count, cache_hit_rate, cache_hits, cache_misses)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#
            )
            .bind(format!("at://test{i}.bsky.social/app.bsky.feed.post/1"))
            .bind(format!("did:plc:test{i}"))
            .bind(1000i64 + i as i64)
            .bind(r#"{"foo":"bar","extra":"data"}"#)
            .bind(r#"{}"#)
            .bind(&old_time_str)
            .bind(&now_str)
            .bind(100i64)
            .bind(1i64)
            .bind(0.5)
            .bind(10i64)
            .bind(10i64)
            .execute(&store.pool)
            .await
            .unwrap();
        }

        let size_before = store.get_db_size().await.unwrap();
        assert!(size_before > 0, "DB should have size");

        let max_size = size_before / 2;
        let result = store
            .cleanup_with_vacuum(7, max_size, 1024, 1.0, 1000, 50)
            .await
            .unwrap();

        assert!(
            result.records_deleted > 0,
            "Should have deleted some records"
        );

        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_cleanup_with_vacuum_under_limit() {
        let store = create_test_db().await;

        let now = Utc::now();
        let now_str = now.to_rfc3339();

        for i in 0..3 {
            sqlx::query(
                r#"INSERT INTO records (at_uri, did, time_us, message, message_metadata, created_at, hydrated_at, hydration_time_ms, api_calls_count, cache_hit_rate, cache_hits, cache_misses)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#
            )
            .bind(format!("at://recent{i}.bsky.social/app.bsky.feed.post/1"))
            .bind(format!("did:plc:recent{i}"))
            .bind(1000i64 + i as i64)
            .bind(r#"{"foo":"bar"}"#)
            .bind(r#"{}"#)
            .bind(&now_str)
            .bind(&now_str)
            .bind(100i64)
            .bind(1i64)
            .bind(0.5)
            .bind(10i64)
            .bind(10i64)
            .execute(&store.pool)
            .await
            .unwrap();
        }

        let large_size = 100_000_000_000i64;
        let result = store
            .cleanup_with_vacuum(7, large_size, 1024, 1.0, 1000, 50)
            .await
            .unwrap();

        assert_eq!(
            result.records_deleted, 0,
            "Should not delete anything when under limit"
        );

        store.close().await.unwrap();
    }
}
