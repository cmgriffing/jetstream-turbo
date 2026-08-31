use crate::models::{
    enriched::EnrichedRecord,
    recovery::{IngestionCheckpoint, SourceCursor, SourceEventId},
    TurboError, TurboResult,
};
use crate::storage::schema::{
    reconcile_required_indexes, verify_required_indexes, SchemaMaintenanceError,
    SchemaMaintenanceReport, SchemaVerification,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use simd_json::to_writer as simd_json_to_writer;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    Connection, Row, SqliteConnection, SqlitePool,
};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;
use tokio::time::{sleep, Duration};
use tracing::{info, instrument, trace, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VacuumPendingReason {
    /// The database file exceeded `max_db_size_mb` after the retention delete loop.
    OverBudget,
    /// The freelist ratio exceeded `vacuum_freelist_ratio` while under the size limit.
    Bloat,
}

/// Execution mode for VACUUM. `FileBackedTempStore` runs VACUUM on a dedicated
/// maintenance connection whose temp store is file-backed, so the transient
/// copy cannot allocate database-sized process memory. `PooledMemory` is the
/// legacy behavior (plain `VACUUM` on a pooled `temp_store = MEMORY`
/// connection) retained as an explicit operator rollback switch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VacuumExecutionMode {
    /// Dedicated maintenance connection with `PRAGMA temp_store = FILE`.
    #[default]
    FileBackedTempStore,
    /// Legacy pooled-connection VACUUM (unbounded transient memory).
    PooledMemory,
}

impl VacuumExecutionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FileBackedTempStore => "file_backed_temp_store",
            Self::PooledMemory => "pooled_memory",
        }
    }
}

/// Why the scheduler deferred a pending VACUUM (or force-ran it past the
/// maximum-deferral window).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VacuumGatingReason {
    /// Memory pressure was elevated at scheduling time.
    MemoryPressure,
    /// The pipeline was in an active replay/catch-up phase.
    RecoveryPhase,
    /// The current UTC hour fell outside the configured low-traffic window.
    Window,
    /// The scheduler force-ran the VACUUM past `vacuum_max_defer_hours` or by
    /// operator override.
    ForceDefer,
}

impl VacuumGatingReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MemoryPressure => "memory_pressure",
            Self::RecoveryPhase => "recovery_phase",
            Self::Window => "window",
            Self::ForceDefer => "force_defer",
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct VacuumState {
    pub pending: bool,
    pub pending_reason: Option<VacuumPendingReason>,
    pub pending_since: Option<DateTime<Utc>>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_run_duration_ms: Option<u64>,
    pub last_run_bytes_reclaimed: Option<i64>,
    /// Whether the database file was over budget at the last cleanup evaluation.
    pub over_budget: bool,
    /// Whether the database file was still over budget after the most recent VACUUM.
    pub over_budget_after_vacuum: bool,
    /// Why the scheduler currently defers a pending VACUUM, if it does.
    pub gating_reason: Option<VacuumGatingReason>,
    /// Seconds the current pending VACUUM has been deferred so far.
    pub deferred_seconds: Option<u64>,
    /// Reason recorded at the most recent forced (past max-defer) VACUUM run.
    pub last_forced_reason: Option<VacuumGatingReason>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VacuumRunResult {
    pub started_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub size_before: i64,
    pub size_after: i64,
    pub bytes_reclaimed: i64,
    pub over_budget_after_vacuum: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanupResult {
    pub records_deleted: u64,
    pub new_size_bytes: i64,
    pub vacuum_pending: bool,
    pub vacuum_pending_reason: Option<VacuumPendingReason>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SQLiteStateSnapshot {
    pub db_size_bytes: i64,
    pub wal_size_bytes: Option<i64>,
    pub page_count: i64,
    pub page_size_bytes: i64,
    pub freelist_count: i64,
    pub freelist_ratio: Option<f64>,
    pub cache_size_pages: i64,
    pub mmap_size_bytes: i64,
    pub journal_mode: String,
    pub journal_size_limit_bytes: i64,
    pub temp_store: i64,
    pub partial_records: i64,
    pub vacuum_pending: bool,
    pub vacuum_pending_reason: Option<VacuumPendingReason>,
    pub vacuum_pending_since: Option<DateTime<Utc>>,
    pub vacuum_last_run_at: Option<DateTime<Utc>>,
    pub vacuum_last_run_duration_ms: Option<u64>,
    pub vacuum_last_run_bytes_reclaimed: Option<i64>,
    pub over_budget: bool,
    pub over_budget_after_vacuum: bool,
    pub vacuum_gating_reason: Option<VacuumGatingReason>,
    pub vacuum_deferred_seconds: Option<u64>,
    pub vacuum_last_forced_reason: Option<VacuumGatingReason>,
}

#[derive(Debug, Clone, Copy)]
pub struct SQLitePragmaConfig {
    pub cache_size_kib: u32,
    pub mmap_size_mb: u64,
    pub journal_size_limit_mb: u64,
}

/// Execution policy for a single VACUUM run.
#[derive(Debug, Clone)]
pub struct VacuumRunPolicy {
    pub mode: VacuumExecutionMode,
    /// Directory on the temp volume for file-backed transient state.
    pub temp_dir: std::path::PathBuf,
    /// VACUUM is refused in pooled-memory mode above this database size.
    pub max_in_memory_db_bytes: i64,
}

impl Default for VacuumRunPolicy {
    fn default() -> Self {
        Self {
            mode: VacuumExecutionMode::default(),
            temp_dir: std::env::temp_dir(),
            // 2 GiB: pooled-memory VACUUM above this readily risks the
            // recovery threshold on the 8 GiB host baseline.
            max_in_memory_db_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
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
    max_connections: u32,
    pragma_config: SQLitePragmaConfig,
    vacuum_state: Mutex<VacuumState>,
}

impl SQLiteStore {
    /// Performs the same bounded compatibility and index verification used by serve startup.
    pub async fn verify_schema_ready<P: AsRef<Path>>(
        db_path: P,
        pragma_config: SQLitePragmaConfig,
    ) -> TurboResult<()> {
        let db_path_str = db_path.as_ref().to_string_lossy().to_string();
        if db_path_str != ":memory:" {
            if let Some(parent) = Path::new(&db_path_str).parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        let pool =
            Self::connect_pool(&db_path_str, pragma_config, Duration::from_secs(5), 1).await?;
        Self::initialize_bounded_schema(&pool).await?;
        let verification = verify_required_indexes(&pool).await?;
        pool.close().await;
        match verification {
            SchemaVerification::Ready => Ok(()),
            SchemaVerification::MaintenanceRequired {
                missing_indexes,
                incompatible_indexes,
            } => Err(TurboError::SchemaMaintenanceRequired {
                missing_indexes,
                incompatible_indexes,
            }),
        }
    }

    pub async fn new<P: AsRef<Path>>(
        db_path: P,
        pragma_config: SQLitePragmaConfig,
    ) -> TurboResult<Self> {
        Self::new_with_pool_limit(db_path, pragma_config, 5).await
    }

    pub async fn new_with_pool_limit<P: AsRef<Path>>(
        db_path: P,
        pragma_config: SQLitePragmaConfig,
        max_connections: u32,
    ) -> TurboResult<Self> {
        let db_path_str = db_path.as_ref().to_string_lossy().to_string();

        info!("Creating SQLite database at: {}", db_path_str);

        // Ensure parent directory exists (skip for in-memory databases)
        if db_path_str != ":memory:" {
            if let Some(parent) = Path::new(&db_path_str).parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }

        let pool = Self::connect_pool(
            &db_path_str,
            pragma_config,
            Duration::from_secs(5),
            max_connections.max(1),
        )
        .await?;

        Self::initialize_bounded_schema(&pool).await?;
        match verify_required_indexes(&pool).await? {
            SchemaVerification::Ready => {}
            SchemaVerification::MaintenanceRequired {
                missing_indexes,
                incompatible_indexes,
            } => {
                return Err(TurboError::SchemaMaintenanceRequired {
                    missing_indexes,
                    incompatible_indexes,
                });
            }
        }

        Ok(Self {
            pool,
            db_path: db_path_str,
            max_connections: max_connections.max(1),
            pragma_config,
            vacuum_state: Mutex::new(VacuumState::default()),
        })
    }

    /// Runs explicit, retry-safe schema maintenance against a database path.
    pub async fn maintain_schema<P: AsRef<Path>>(
        db_path: P,
        pragma_config: SQLitePragmaConfig,
        busy_timeout: Duration,
    ) -> Result<SchemaMaintenanceReport, SchemaMaintenanceError> {
        let db_path_str = db_path.as_ref().to_string_lossy().to_string();
        if db_path_str != ":memory:" {
            if let Some(parent) = Path::new(&db_path_str).parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|source| SchemaMaintenanceError::Preparation(Box::new(source)))?;
            }
        }

        let command_started = Instant::now();
        let pool = Self::connect_pool(&db_path_str, pragma_config, busy_timeout, 1)
            .await
            .map_err(|source| maintenance_sql_error("database_connection", busy_timeout, source))?;
        Self::initialize_bounded_schema(&pool)
            .await
            .map_err(|source| {
                maintenance_sql_error("bounded_schema_setup", busy_timeout, source)
            })?;

        let report = reconcile_required_indexes(&pool, busy_timeout).await;
        let report = match report {
            Ok(report) => report,
            Err(error) => {
                tracing::error!(
                    error = %error,
                    outcome = "failure",
                    "Schema maintenance command failed"
                );
                return Err(error);
            }
        };

        info!(
            created_indexes = report.created_indexes.len(),
            skipped_indexes = report.skipped_indexes.len(),
            elapsed_ms = command_started.elapsed().as_millis() as u64,
            outcome = "success",
            "Schema maintenance command completed"
        );
        Ok(report)
    }

    async fn connect_pool(
        db_path: &str,
        pragma_config: SQLitePragmaConfig,
        busy_timeout: Duration,
        max_connections: u32,
    ) -> Result<SqlitePool, sqlx::Error> {
        let mut connect_options = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .busy_timeout(busy_timeout);

        if db_path != ":memory:" {
            connect_options = connect_options.journal_mode(SqliteJournalMode::Wal);
        }

        SqlitePoolOptions::new()
            .max_connections(max_connections.max(1))
            .after_connect({
                let db_path = db_path.to_string();
                move |conn, _meta| {
                    let db_path = db_path.clone();
                    Box::pin(async move {
                        Self::apply_pragmas(conn, pragma_config, &db_path).await?;
                        Ok(())
                    })
                }
            })
            .connect_with(connect_options)
            .await
    }

    async fn initialize_bounded_schema(pool: &SqlitePool) -> Result<(), sqlx::Error> {
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
        trace!("Bounded SQLite schema compatibility checks completed");
        Ok(())
    }

    async fn apply_pragmas(
        conn: &mut SqliteConnection,
        pragma_config: SQLitePragmaConfig,
        db_path: &str,
    ) -> Result<(), sqlx::Error> {
        Self::apply_pragmas_with_temp_store(conn, pragma_config, db_path, "MEMORY").await
    }

    async fn apply_pragmas_with_temp_store(
        conn: &mut SqliteConnection,
        pragma_config: SQLitePragmaConfig,
        db_path: &str,
        temp_store: &str,
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

        sqlx::query(&format!("PRAGMA temp_store = {temp_store}"))
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

        // Serialize both JSON columns into one buffer (one allocation instead
        // of two), then bind the two non-overlapping UTF-8 slices.
        let mut buf = Vec::with_capacity(1024);
        record.message.write_json(&mut buf);
        let message_end = buf.len();
        simd_json_to_writer(&mut buf, &record.hydrated_metadata).unwrap();
        let combined = String::from_utf8(buf).expect("serialized JSON is UTF-8");
        let message_json = &combined[..message_end];
        let metadata_json = &combined[message_end..];

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

        let message = serde_json::from_value(message)?;
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

    /// Deletes records older than the cutoff in bounded chunks. `pause` is
    /// consulted before every chunk after the first; when it returns true
    /// (e.g. memory pressure is elevated) the loop yields and the caller
    /// resumes on the next scheduling cycle without exceeding the existing
    /// chunk-delay backoff behavior.
    pub async fn cleanup_old_records(
        &self,
        older_than: DateTime<Utc>,
        chunk_size: u32,
        chunk_delay_ms: u64,
        pause: Option<&(dyn Fn() -> bool + Send + Sync)>,
    ) -> TurboResult<u64> {
        let older_than_str = older_than.to_rfc3339();
        let mut total_deleted = 0u64;

        loop {
            if total_deleted > 0 && pause.is_some_and(|pause| pause()) {
                info!(
                    total_deleted,
                    "Cleanup chunk loop paused (gating callback signaled); resuming next cycle"
                );
                break;
            }
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
        let (temp_store,): (i64,) = sqlx::query_as("PRAGMA temp_store")
            .fetch_one(&self.pool)
            .await?;
        let freelist_ratio = if page_count > 0 {
            Some(freelist_count as f64 / page_count as f64)
        } else {
            None
        };
        let vacuum_state = self.get_vacuum_state();
        // The deferral age is derived at read time so diagnostics stay
        // truthful between scheduler ticks.
        let vacuum_deferred_seconds = vacuum_state
            .pending_since
            .filter(|_| vacuum_state.pending)
            .map(|since| {
                Utc::now()
                    .signed_duration_since(since)
                    .num_seconds()
                    .max(0) as u64
            });

        Ok(SQLiteStateSnapshot {
            db_size_bytes,
            wal_size_bytes,
            page_count,
            page_size_bytes,
            freelist_count,
            freelist_ratio,
            cache_size_pages,
            mmap_size_bytes,
            journal_mode,
            journal_size_limit_bytes,
            temp_store,
            partial_records,
            vacuum_pending: vacuum_state.pending,
            vacuum_pending_reason: vacuum_state.pending_reason,
            vacuum_pending_since: vacuum_state.pending_since,
            vacuum_last_run_at: vacuum_state.last_run_at,
            vacuum_last_run_duration_ms: vacuum_state.last_run_duration_ms,
            vacuum_last_run_bytes_reclaimed: vacuum_state.last_run_bytes_reclaimed,
            over_budget: vacuum_state.over_budget,
            over_budget_after_vacuum: vacuum_state.over_budget_after_vacuum,
            vacuum_gating_reason: vacuum_state.gating_reason,
            vacuum_deferred_seconds: vacuum_state
                .deferred_seconds
                .or(vacuum_deferred_seconds),
            vacuum_last_forced_reason: vacuum_state.last_forced_reason,
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

    /// Connects a dedicated single maintenance connection for VACUUM that does
    /// not inherit the pool's `temp_store = MEMORY` pragma: transient VACUUM
    /// state is therefore file-backed and bounded by the page cache instead of
    /// the database size.
    async fn connect_maintenance_connection(&self) -> TurboResult<SqliteConnection> {
        let mut connect_options = SqliteConnectOptions::new()
            .filename(&self.db_path)
            .busy_timeout(Duration::from_secs(30));
        if self.db_path != ":memory:" {
            connect_options = connect_options.journal_mode(SqliteJournalMode::Wal);
        }
        let mut conn = SqliteConnection::connect_with(&connect_options).await?;
        Self::apply_pragmas_with_temp_store(&mut conn, self.pragma_config, &self.db_path, "FILE")
            .await?;
        Ok(conn)
    }

    /// Verifies the temp volume holds enough free space for a transient copy
    /// of the database (VACUUM's file-backed working set is bounded by the
    /// freelist, which cannot exceed the database size).
    pub fn verify_temp_volume_headroom(dir: &Path, required_bytes: i64) -> TurboResult<()> {
        let available = fs4::available_space(dir).map_err(|error| {
            crate::models::TurboError::Internal(format!(
                "unable to read free space on vacuum temp volume {}: {error}",
                dir.display()
            ))
        })?;
        let required = u64::try_from(required_bytes).unwrap_or(u64::MAX);
        if available < required {
            return Err(crate::models::TurboError::Internal(format!(
                "vacuum temp volume {} has only {} free bytes; {} bytes are required \
                 for a transient copy of the database",
                dir.display(),
                available,
                required
            )));
        }
        if available < required.saturating_mul(2) {
            warn!(
                available_bytes = available,
                required_bytes = required,
                "vacuum temp volume headroom is below 2x the database size; VACUUM may fail if growth continues"
            );
        }
        Ok(())
    }

    /// Records that the scheduler deferred a pending VACUUM for `reason`, so
    /// diagnostics and gauges report a truthful gating state.
    pub fn record_vacuum_deferral(&self, reason: VacuumGatingReason) {
        let mut state = self
            .vacuum_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.gating_reason = Some(reason);
    }

    /// Records that a VACUUM was force-run past the deferral window (or by
    /// operator override), keeping the reason observable after the state clears.
    pub fn record_vacuum_forced(&self) {
        let mut state = self
            .vacuum_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.last_forced_reason = Some(VacuumGatingReason::ForceDefer);
    }

    pub async fn cleanup_with_vacuum(
        &self,
        retention_days: u32,
        max_size_bytes: i64,
        vacuum_freelist_ratio: f64,
        cleanup_chunk_size: u32,
        cleanup_chunk_delay_ms: u64,
        chunk_pause: Option<&(dyn Fn() -> bool + Send + Sync)>,
    ) -> TurboResult<CleanupResult> {
        let initial_size = self.get_db_size().await?;
        let mut total_deleted: u64 = 0;

        // The retention delete loop only runs when the file is over budget:
        // SQLite (auto_vacuum = NONE) never shrinks the file on DELETE, so
        // deleting below the limit is pointless work that only grows the
        // freelist. Size only decreases via VACUUM.
        if initial_size > max_size_bytes {
            let mut current_retention = retention_days;
            let max_iterations = 3;

            for iteration in 0..max_iterations {
                let cutoff = Utc::now() - chrono::Duration::days(current_retention as i64);
                let deleted = self
                    .cleanup_old_records(
                        cutoff,
                        cleanup_chunk_size,
                        cleanup_chunk_delay_ms,
                        chunk_pause,
                    )
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
        }

        let post_delete_size = self.get_db_size().await?;
        let vacuum_pending_reason = self
            .decide_vacuum_pending(max_size_bytes, vacuum_freelist_ratio)
            .await?;

        Ok(CleanupResult {
            records_deleted: total_deleted,
            new_size_bytes: post_delete_size,
            vacuum_pending: vacuum_pending_reason.is_some(),
            vacuum_pending_reason,
        })
    }

    /// Under-budget cleanup cycle: evaluate only the proactive freelist-bloat
    /// check without deleting any records.
    pub async fn check_vacuum_bloat(
        &self,
        max_size_bytes: i64,
        vacuum_freelist_ratio: f64,
    ) -> TurboResult<Option<VacuumPendingReason>> {
        self.decide_vacuum_pending(max_size_bytes, vacuum_freelist_ratio)
            .await
    }

    /// Decides whether a VACUUM should be pending: over budget after the delete
    /// loop, or freelist ratio above the configured threshold while under the
    /// size limit. Records the decision in `vacuum_state`; an existing pending
    /// flag is preserved (with its original pending-since timestamp) while the
    /// same reason still applies, so the scheduler's defer/escalation logic is
    /// stable across cycles.
    async fn decide_vacuum_pending(
        &self,
        max_size_bytes: i64,
        vacuum_freelist_ratio: f64,
    ) -> TurboResult<Option<VacuumPendingReason>> {
        let db_size = self.get_db_size().await?;
        let reason = if db_size > max_size_bytes {
            Some(VacuumPendingReason::OverBudget)
        } else {
            let (freelist_count,): (i64,) = sqlx::query_as("PRAGMA freelist_count")
                .fetch_one(&self.pool)
                .await?;
            let (page_count,): (i64,) = sqlx::query_as("PRAGMA page_count")
                .fetch_one(&self.pool)
                .await?;
            let ratio = if page_count > 0 {
                freelist_count as f64 / page_count as f64
            } else {
                0.0
            };
            if ratio > vacuum_freelist_ratio {
                Some(VacuumPendingReason::Bloat)
            } else {
                None
            }
        };

        let mut state = self
            .vacuum_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.over_budget = db_size > max_size_bytes;
        if let Some(reason) = reason {
            if !state.pending || state.pending_reason != Some(reason) {
                state.pending = true;
                state.pending_reason = Some(reason);
                state.pending_since = Some(Utc::now());
            }
            info!(
                ?reason,
                size_mb = db_size / (1024 * 1024),
                max_size_mb = max_size_bytes / (1024 * 1024),
                "VACUUM scheduled (pending)"
            );
        } else {
            trace!(
                size_mb = db_size / (1024 * 1024),
                max_size_mb = max_size_bytes / (1024 * 1024),
                "No VACUUM scheduled this cycle"
            );
        }
        drop(state);
        Ok(reason)
    }

    /// Runs a VACUUM in the configured execution mode, recording the duration
    /// and bytes reclaimed from the file-size delta. Clears any pending flag on
    /// success and tracks whether the file is still over budget afterwards.
    ///
    /// `FileBackedTempStore` runs on a dedicated maintenance connection with
    /// `PRAGMA temp_store = FILE`, bounding transient memory to SQLite page-
    /// cache size instead of database size. The file-backed mode also verifies
    /// that the temp volume has headroom for a transient copy of the database;
    /// `db_path`'s parent directory is used as the fallback temp volume.
    /// `PooledMemory` retains the legacy pooled-connection behavior and is
    /// refused above the safety threshold.
    pub async fn run_vacuum(
        &self,
        max_size_bytes: i64,
        policy: &VacuumRunPolicy,
    ) -> TurboResult<VacuumRunResult> {
        let started_at = Utc::now();
        let start = Instant::now();
        let size_before = self.get_db_size().await?;

        match policy.mode {
            VacuumExecutionMode::FileBackedTempStore => {
                Self::verify_temp_volume_headroom(&policy.temp_dir, size_before)?;
                let mut conn = self.connect_maintenance_connection().await?;
                if let Err(e) = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
                    .execute(&mut conn)
                    .await
                {
                    warn!("wal_checkpoint(TRUNCATE) before VACUUM failed: {e}; continuing");
                }
                sqlx::query("VACUUM").execute(&mut conn).await?;
                conn.close().await?;
            }
            VacuumExecutionMode::PooledMemory => {
                if size_before > policy.max_in_memory_db_bytes {
                    return Err(crate::models::TurboError::Internal(format!(
                        "refusing VACUUM with temp_store = MEMORY: database is {} bytes, \
                         above the pooled-memory safety threshold of {} bytes",
                        size_before, policy.max_in_memory_db_bytes
                    )));
                }
                let mut conn = self.pool.acquire().await?;
                if let Err(e) = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
                    .execute(&mut *conn)
                    .await
                {
                    warn!("wal_checkpoint(TRUNCATE) before VACUUM failed: {e}; continuing");
                }
                sqlx::query("VACUUM").execute(&mut *conn).await?;
                drop(conn);
            }
        }

        let duration = start.elapsed();
        let duration_ms = duration.as_millis() as u64;
        let size_after = self.get_db_size().await?;
        let bytes_reclaimed = size_before.saturating_sub(size_after);
        let over_budget_after_vacuum = size_after > max_size_bytes;

        {
            let mut state = self
                .vacuum_state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.pending = false;
            state.pending_reason = None;
            state.pending_since = None;
            state.gating_reason = None;
            state.deferred_seconds = None;
            state.last_run_at = Some(started_at);
            state.last_run_duration_ms = Some(duration_ms);
            state.last_run_bytes_reclaimed = Some(bytes_reclaimed);
            state.over_budget = over_budget_after_vacuum;
            state.over_budget_after_vacuum = over_budget_after_vacuum;
        }

        info!(
            size_before_mb = size_before / (1024 * 1024),
            size_after_mb = size_after / (1024 * 1024),
            bytes_reclaimed,
            duration_ms,
            over_budget_after_vacuum,
            "VACUUM completed"
        );

        Ok(VacuumRunResult {
            started_at,
            duration_ms,
            size_before,
            size_after,
            bytes_reclaimed,
            over_budget_after_vacuum,
        })
    }

    pub fn get_vacuum_state(&self) -> VacuumState {
        self.vacuum_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub async fn get_db_path(&self) -> &str {
        &self.db_path
    }

    pub fn pool_memory_snapshot(&self) -> SQLitePoolMemorySnapshot {
        let cache_bytes_per_connection =
            u64::from(self.pragma_config.cache_size_kib).saturating_mul(1024);
        SQLitePoolMemorySnapshot {
            size: self.pool.size(),
            idle: self.pool.num_idle(),
            max_connections: self.max_connections,
            cache_bytes_per_connection,
            aggregate_cache_limit_bytes: cache_bytes_per_connection
                .saturating_mul(u64::from(self.max_connections)),
            mmap_limit_bytes: self.pragma_config.mmap_size_mb.saturating_mul(1024 * 1024),
            temp_store: "memory",
        }
    }

    pub async fn close(&self) -> TurboResult<()> {
        self.pool.close().await;
        info!("SQLite connection pool closed");
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SQLitePoolMemorySnapshot {
    pub size: u32,
    pub idle: usize,
    pub max_connections: u32,
    pub cache_bytes_per_connection: u64,
    pub aggregate_cache_limit_bytes: u64,
    pub mmap_limit_bytes: u64,
    pub temp_store: &'static str,
}

fn maintenance_sql_error(
    operation: &str,
    busy_timeout: Duration,
    source: sqlx::Error,
) -> SchemaMaintenanceError {
    let is_lock_contention = match &source {
        sqlx::Error::Database(database_error) => {
            matches!(database_error.code().as_deref(), Some("5" | "6"))
                || database_error.message().contains("database is locked")
        }
        _ => false,
    };
    if is_lock_contention {
        SchemaMaintenanceError::LockTimeout {
            index: operation.to_string(),
            timeout: busy_timeout,
            source: Box::new(source),
        }
    } else {
        SchemaMaintenanceError::Preparation(Box::new(source))
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

            // Serialize every row's message+metadata into one owned buffer each
            // (a single allocation per row instead of two), then bind the two
            // non-overlapping UTF-8 slices. The buffers are collected before the
            // binds so sqlx's borrowed `&str` arguments outlive the query.
            let mut serialized: Vec<(String, usize)> = Vec::with_capacity(chunk.len());
            for record in chunk {
                let mut buf = Vec::with_capacity(1024);
                record.message.write_json(&mut buf);
                let message_end = buf.len();
                simd_json_to_writer(&mut buf, &record.hydrated_metadata).unwrap();
                serialized.push((String::from_utf8(buf).expect("JSON is UTF-8"), message_end));
            }

            for (record, (combined, message_end)) in chunk.iter().zip(&serialized) {
                let message_json = &combined[..*message_end];
                let metadata_json = &combined[*message_end..];

                query = query
                    .bind(record.get_at_uri())
                    .bind(record.get_did())
                    .bind(record.message.time_us.map(|t| t as i64))
                    .bind(record.source_event_id().to_string())
                    .bind(message_json)
                    .bind(metadata_json)
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
        let pragma_config = test_pragma_config();
        maintain_test_db(&db_path, pragma_config).await;
        SQLiteStore::new(&db_path, pragma_config).await.unwrap()
    }

    fn test_pragma_config() -> SQLitePragmaConfig {
        SQLitePragmaConfig {
            cache_size_kib: 64 * 1024,
            mmap_size_mb: 256,
            journal_size_limit_mb: 512,
        }
    }

    async fn maintain_test_db(path: &Path, pragma_config: SQLitePragmaConfig) {
        SQLiteStore::maintain_schema(path, pragma_config, std::time::Duration::from_secs(1))
            .await
            .unwrap();
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
                did: "did:plc:stored".to_string().into(),
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

        let pragma_config = test_pragma_config();
        maintain_test_db(&db_path, pragma_config).await;
        let store = SQLiteStore::new(&db_path, pragma_config).await.unwrap();

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

        let pragma_config = test_pragma_config();
        maintain_test_db(&db_path, pragma_config).await;
        let store = SQLiteStore::new(&db_path, pragma_config).await.unwrap();
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
    async fn crash_inside_coalescing_window_replays_and_filters_committed_events() {
        // A batch completed and its records were stored, but the durable
        // checkpoint write was coalesced away before the crash: after the
        // restart (empty durable checkpoint) the same source events are
        // recognized as already-committed duplicates and must be filtered
        // (never re-published), and replaying to completion advances the
        // durable checkpoint past the coalescing window.
        let store = create_test_db().await;
        let first = test_record(1);
        let second = test_record(2);
        let first_id = first.source_event_id();
        let second_id = second.source_event_id();
        store.store_batch(&[first, second]).await.unwrap();

        // Crash before the coalesced checkpoint write: no durable checkpoint.
        assert_eq!(store.load_ingestion_checkpoint().await.unwrap(), None);

        // Restarted replay deduplicates against committed work.
        let completed = store
            .completed_source_event_ids(&[first_id.clone(), second_id.clone()])
            .await
            .unwrap();
        assert_eq!(
            completed,
            HashSet::from([first_id, second_id]),
            "committed events must be filtered as duplicates, not re-published"
        );

        // Replay of the coalescing window completes contiguously and out of
        // the durability path: the durable checkpoint advances.
        let resumed = test_checkpoint(2, 2_000);
        assert!(store.advance_ingestion_checkpoint(&resumed).await.unwrap());
        assert_eq!(
            store.load_ingestion_checkpoint().await.unwrap(),
            Some(resumed)
        );
    }

    #[tokio::test]
    async fn vacuum_pooled_memory_mode_refuses_above_safety_threshold() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("pooled-vacuum.db");
        let pragma_config = test_pragma_config();
        maintain_test_db(&db_path, pragma_config).await;
        let store = SQLiteStore::new(&db_path, pragma_config).await.unwrap();

        // Tiny safety threshold: the pooled-memory mode must refuse rather
        // than allocate unbounded transient memory, and vacuum stays pending.
        let size = store.get_db_size().await.unwrap();
        let policy = VacuumRunPolicy {
            mode: VacuumExecutionMode::PooledMemory,
            temp_dir: temp_dir.path().to_path_buf(),
            max_in_memory_db_bytes: size.saturating_sub(1),
        };
        let error = store
            .run_vacuum(i64::MAX, &policy)
            .await
            .expect_err("pooled-memory VACUUM above threshold must refuse");
        assert!(error.to_string().contains("refusing VACUUM"));
    }

    #[tokio::test]
    async fn vacuum_file_backed_mode_records_gating_and_result() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("file-backed-vacuum.db");
        let pragma_config = test_pragma_config();
        maintain_test_db(&db_path, pragma_config).await;
        let store = SQLiteStore::new(&db_path, pragma_config).await.unwrap();

        // Simulate scheduler gating: a deferral reason must be observable.
        store.record_vacuum_deferral(VacuumGatingReason::RecoveryPhase);
        let snapshot = store.get_state_snapshot().await.unwrap();
        assert_eq!(
            snapshot.vacuum_gating_reason,
            Some(VacuumGatingReason::RecoveryPhase)
        );

        let policy = VacuumRunPolicy {
            mode: VacuumExecutionMode::FileBackedTempStore,
            temp_dir: temp_dir.path().to_path_buf(),
            max_in_memory_db_bytes: 0,
        };
        let run = store
            .run_vacuum(i64::MAX, &policy)
            .await
            .expect("file-backed VACUUM must succeed on a small fixture");
        assert_eq!(run.bytes_reclaimed, 0);

        // After a successful run the gating reason clears and the result is
        // recorded.
        store.record_vacuum_forced();
        let snapshot = store.get_state_snapshot().await.unwrap();
        assert_eq!(snapshot.vacuum_gating_reason, None);
        assert_eq!(
            snapshot.vacuum_last_forced_reason,
            Some(VacuumGatingReason::ForceDefer)
        );
        assert!(snapshot.vacuum_last_run_at.is_some());
        assert!(snapshot.vacuum_last_run_duration_ms.is_some());
    }

    #[tokio::test]
    async fn cleanup_old_records_pauses_between_chunks_when_gated() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("pause-cleanup.db");
        let pragma_config = test_pragma_config();
        maintain_test_db(&db_path, pragma_config).await;
        let store = SQLiteStore::new(&db_path, pragma_config).await.unwrap();

        // Insert 250 backdated records so retention cleanup targets them.
        let old_time_str = (Utc::now() - Duration::days(10)).to_rfc3339();
        let now_str = Utc::now().to_rfc3339();
        for seq in 0..250 {
            sqlx::query(
                r#"INSERT INTO records (
                    at_uri, did, time_us, source_event_id, message, message_metadata,
                    created_at, hydrated_at, hydration_time_ms, api_calls_count,
                    cache_hit_rate, cache_hits, cache_misses
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
            )
            .bind(format!("at://old.bsky.social/app.bsky.feed.post/{seq}"))
            .bind("did:plc:old")
            .bind(seq as i64)
            .bind(format!("old-{seq}"))
            .bind(r#"{"foo":"bar"}"#)
            .bind(r#"{}"#)
            .bind(&old_time_str)
            .bind(&now_str)
            .bind(1i64)
            .bind(1i64)
            .bind(0.5)
            .bind(1i64)
            .bind(1i64)
            .execute(&store.pool)
            .await
            .unwrap();
        }

        // After the first chunk the pause callback signals pressure and the
        // loop yields with partial progress, resuming on the next cycle
        // without exceeding the chunk backoff behavior.
        let pause = || true;
        let deleted_first_cycle = store
            .cleanup_old_records(Utc::now() - Duration::days(1), 100, 0, Some(&pause))
            .await
            .unwrap();
        assert!(deleted_first_cycle > 0 && deleted_first_cycle < 250);
        let remaining = store.count_records().await.unwrap();
        assert!(remaining > 0);

        // Unpaused cycle drains the rest.
        let deleted_second_cycle = store
            .cleanup_old_records(Utc::now() - Duration::days(1), 1000, 0, None)
            .await
            .unwrap();
        assert_eq!(remaining as u64, deleted_second_cycle);
        assert_eq!(store.count_records().await.unwrap(), 0);
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
        maintain_test_db(&db_path, pragma_config).await;
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
        assert!(
            snapshot.freelist_ratio.is_some_and(|ratio| ratio >= 0.0),
            "freelist ratio should be present and non-negative"
        );
        assert!(
            !snapshot.vacuum_pending,
            "no VACUUM should be pending initially"
        );
        assert_eq!(snapshot.vacuum_pending_reason, None);
        assert_eq!(snapshot.vacuum_pending_since, None);
        assert_eq!(snapshot.vacuum_last_run_at, None);
        assert_eq!(snapshot.vacuum_last_run_duration_ms, None);
        assert_eq!(snapshot.vacuum_last_run_bytes_reclaimed, None);
        assert!(!snapshot.over_budget);
        assert!(!snapshot.over_budget_after_vacuum);

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
        let deleted = store.cleanup_old_records(cutoff, 1000, 50, None).await.unwrap();

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
        let deleted = store.cleanup_old_records(cutoff, 1000, 50, None).await.unwrap();

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
            .cleanup_with_vacuum(7, max_size, 0.10, 1000, 50, None)
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
            .cleanup_with_vacuum(7, large_size, 0.10, 1000, 50, None)
            .await
            .unwrap();

        assert_eq!(
            result.records_deleted, 0,
            "Should not delete anything when under limit"
        );
        assert!(
            !result.vacuum_pending,
            "Fresh DB under limit should not schedule a VACUUM"
        );

        store.close().await.unwrap();
    }

    /// Inserts `count` records dated `age_days` in the past so the retention
    /// delete loop treats them as expired.
    async fn insert_expired_records(store: &SQLiteStore, count: u32, age_days: i64) {
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let old_time = now - chrono::Duration::days(age_days);
        let old_time_str = old_time.to_rfc3339();

        for i in 0..count {
            sqlx::query(
                r#"INSERT INTO records (at_uri, did, time_us, message, message_metadata, created_at, hydrated_at, hydration_time_ms, api_calls_count, cache_hit_rate, cache_hits, cache_misses)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#
            )
            .bind(format!("at://vacuum{i}.bsky.social/app.bsky.feed.post/1"))
            .bind(format!("did:plc:vacuum{i}"))
            .bind(1000i64 + i as i64)
            .bind(r#"{"foo":"bar","payload":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}"#)
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
    }

    #[tokio::test]
    async fn over_budget_vacuum_runs_and_actually_shrinks_the_file() {
        let store = create_test_db().await;

        insert_expired_records(&store, 200, 10).await;

        let size_with_data = store.get_db_size().await.unwrap();
        // Force the database over budget, then run the full cleanup path.
        let max_size_bytes = size_with_data / 2;
        let result = store
            .cleanup_with_vacuum(7, max_size_bytes, 0.10, 1000, 0, None)
            .await
            .unwrap();

        assert!(
            result.records_deleted > 0,
            "over-budget cleanup should delete expired records"
        );
        assert!(result.vacuum_pending);
        assert_eq!(
            result.vacuum_pending_reason,
            Some(VacuumPendingReason::OverBudget)
        );

        // DELETE does not shrink the file (auto_vacuum = NONE): the file is
        // still over budget, so VACUUM must be the lever that reclaims space.
        assert!(
            result.new_size_bytes >= size_with_data,
            "DELETE alone must not shrink the database file"
        );

        let run = store.run_vacuum(max_size_bytes, &VacuumRunPolicy::default()).await.unwrap();
        assert!(
            run.size_after < run.size_before,
            "VACUUM must actually shrink the file ({} -> {})",
            run.size_before,
            run.size_after
        );
        assert!(run.bytes_reclaimed > 0);
        assert!(
            !run.over_budget_after_vacuum,
            "VACUUM should bring the file back under the forced budget"
        );

        let state = store.get_vacuum_state();
        assert!(!state.pending, "pending flag must be cleared after VACUUM");
        assert_eq!(state.pending_reason, None);
        assert!(state.last_run_at.is_some());
        assert_eq!(
            state.last_run_duration_ms,
            Some(run.duration_ms),
            "last run duration should be recorded"
        );
        assert_eq!(
            state.last_run_bytes_reclaimed,
            Some(run.bytes_reclaimed),
            "bytes reclaimed should be recorded"
        );
        assert!(!state.over_budget_after_vacuum);

        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn freelist_bloat_schedules_vacuum_even_under_size_limit() {
        let store = create_test_db().await;

        // Fill the DB with records, then delete them all: with auto_vacuum =
        // NONE the file keeps its size and the freed pages sit on the freelist.
        insert_expired_records(&store, 300, 10).await;
        let deleted = store
            .cleanup_old_records(Utc::now() - chrono::Duration::days(1), 1000, 0, None)
            .await
            .unwrap();
        assert!(deleted > 0);

        let size = store.get_db_size().await.unwrap();
        let large_max = size * 2;

        let reason = store.check_vacuum_bloat(large_max, 0.05).await.unwrap();
        assert_eq!(
            reason,
            Some(VacuumPendingReason::Bloat),
            "freelist ratio above threshold must schedule a VACUUM under the size limit"
        );

        let state = store.get_vacuum_state();
        assert!(state.pending);
        assert_eq!(state.pending_reason, Some(VacuumPendingReason::Bloat));
        assert!(state.pending_since.is_some());
        assert!(!state.over_budget);

        // An impossible threshold must not schedule anything.
        let none = store.check_vacuum_bloat(large_max, 1.0).await.unwrap();
        assert_eq!(
            none, None,
            "ratio below threshold must not schedule a VACUUM"
        );

        store.close().await.unwrap();
    }
}
