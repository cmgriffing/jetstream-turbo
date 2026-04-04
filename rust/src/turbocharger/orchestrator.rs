use crate::client::{
    BlueskyAuthClient, BlueskyClient, JetstreamClient, MessageSource, PostFetcher, ProfileFetcher,
};
use crate::config::Settings;
use crate::hydration::{Hydrator, TurboCache};
use crate::models::enriched::EnrichedRecord;
use crate::models::{
    errors::{TurboError, TurboResult},
    jetstream::JetstreamMessage,
};
use crate::storage::{EventPublisher, RecordStore, RedisStore, SQLitePragmaConfig, SQLiteStore};
use crate::telemetry::ErrorReporter;
use futures::StreamExt;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, Semaphore};
use tokio::task::JoinSet;
use tokio::time::{interval, sleep};
use tracing::{error, info, trace};

const BATCH_SIZE: usize = 25;
const MAX_WAIT_TIME_MS: u64 = 200;
const MEMORY_PEAK_WINDOW_SECS: u64 = 24 * 60 * 60;

pub struct TurboCharger<M, P, Po, S, E> {
    settings: Settings,
    message_source: M,
    bluesky_client: Arc<BlueskyClient>,
    hydrator: Hydrator<P, Po>,
    record_store: Arc<S>,
    event_publisher: Arc<E>,
    sqlite_store: Arc<SQLiteStore>,
    redis_store: Arc<RedisStore>,
    semaphore: Arc<Semaphore>,
    broadcast_sender: broadcast::Sender<EnrichedRecord>,
    error_reporter: ErrorReporter,
    memory_peak_window: Mutex<MemoryPeakWindow>,
}

impl TurboCharger<JetstreamClient, BlueskyClient, BlueskyClient, SQLiteStore, RedisStore> {
    pub async fn new(
        settings: Settings,
        modulo: u32,
        shard: u32,
        error_reporter: ErrorReporter,
    ) -> TurboResult<Self> {
        info!(
            "Initializing TurboCharger with modulo={}, shard={}",
            modulo, shard
        );

        // Initialize Jetstream client
        let jetstream_client = JetstreamClient::with_defaults(settings.jetstream_hosts.clone())
            .with_channel_capacity(settings.channel_capacity);

        // Authenticate directly with Bluesky
        let auth_client = Arc::new(BlueskyAuthClient::new(
            settings.bluesky_handle.clone(),
            settings.bluesky_app_password.clone(),
        )?);

        let auth_response = auth_client.authenticate().await?;
        info!(
            "Successfully authenticated with Bluesky as {}",
            settings.bluesky_handle
        );
        let bluesky_client = Arc::new(BlueskyClient::new(
            vec![auth_response.access_jwt.clone()],
            Some(auth_client.clone()),
            settings.profile_batch_size,
            settings.post_batch_size,
            settings.profile_batch_wait_ms,
            settings.post_batch_wait_ms,
        )?);
        bluesky_client
            .refresh_sessions(
                vec![auth_response.access_jwt],
                Some(auth_response.refresh_jwt),
                auth_response.expires_at,
            )
            .await;

        // Initialize cache
        let cache = TurboCache::new(settings.cache_size_users, settings.cache_size_posts);

        // Initialize hydrator
        let hydrator = Hydrator::new(cache, bluesky_client.clone(), bluesky_client.clone());

        // Initialize storage
        let db_path = format!("{}/jetstream.db", settings.db_dir);
        let sqlite_store = Arc::new(
            SQLiteStore::new(
                &db_path,
                SQLitePragmaConfig {
                    cache_size_kib: settings.sqlite_cache_size_kib,
                    mmap_size_mb: settings.sqlite_mmap_size_mb,
                    journal_size_limit_mb: settings.sqlite_journal_size_limit_mb,
                },
            )
            .await?,
        );

        let redis_store = Arc::new(
            RedisStore::new(
                &settings.redis_url,
                settings.stream_name_redis.clone(),
                settings.trim_maxlen,
            )
            .await?,
        );

        // Initialize semaphore for concurrency control
        let semaphore = Arc::new(Semaphore::new(
            settings.max_concurrent_requests.max(1) as usize
        ));

        // Initialize broadcast channel
        let (broadcast_sender, _) = broadcast::channel(1000);

        info!("TurboCharger initialized successfully");

        Ok(Self {
            settings,
            message_source: jetstream_client,
            bluesky_client,
            hydrator,
            record_store: sqlite_store.clone(),
            event_publisher: redis_store.clone(),
            sqlite_store,
            redis_store,
            semaphore,
            broadcast_sender,
            error_reporter,
            memory_peak_window: Mutex::new(MemoryPeakWindow::new(MEMORY_PEAK_WINDOW_SECS)),
        })
    }
}

impl<M, P, Po, S, E> TurboCharger<M, P, Po, S, E>
where
    M: MessageSource + Send + Sync + 'static,
    P: ProfileFetcher + Send + Sync + 'static,
    Po: PostFetcher + Send + Sync + 'static,
    S: RecordStore + Send + Sync + 'static,
    E: EventPublisher + Send + Sync + 'static,
{
    pub async fn run(&self) -> TurboResult<()> {
        info!("Starting TurboCharger main loop");

        let message_stream = self.message_source.stream_messages().await?;

        let mut last_stats = std::time::Instant::now();
        let mut buffer: Vec<JetstreamMessage> = Vec::with_capacity(BATCH_SIZE);
        let mut flush_interval = interval(Duration::from_millis(MAX_WAIT_TIME_MS));
        let mut batch_buffer: Vec<JetstreamMessage> = Vec::with_capacity(BATCH_SIZE);
        let mut batch_tasks: JoinSet<TurboResult<usize>> = JoinSet::new();

        tokio::pin!(message_stream);

        loop {
            tokio::select! {
                result = message_stream.next() => {
                    match result {
                        Some(Ok(message)) => {
                            if self.should_process_message(&message) {
                                buffer.push(message);
                            }

                            if buffer.len() >= BATCH_SIZE {
                                // Reuse batch_buffer to avoid allocation
                                batch_buffer.clear();
                                batch_buffer.extend(buffer.drain(..));
                                self.spawn_batch_processing(
                                    std::mem::take(&mut batch_buffer),
                                    &mut batch_tasks,
                                )
                                .await?;
                            }
                        }
                        Some(Err(e)) => {
                            error!("Error receiving message from Jetstream: {}", e);
                        }
                        None => break,
                    }
                }
                _ = flush_interval.tick() => {
                    if !buffer.is_empty() {
                        // Reuse batch_buffer to avoid allocation
                        batch_buffer.clear();
                        batch_buffer.extend(buffer.drain(..));
                        self.spawn_batch_processing(
                            std::mem::take(&mut batch_buffer),
                            &mut batch_tasks,
                        )
                        .await?;
                    }
                }
            }

            while let Some(task_result) = batch_tasks.try_join_next() {
                self.handle_batch_task_result(task_result)?;
            }

            if last_stats.elapsed() >= Duration::from_secs(30) {
                let process_memory = collect_process_memory_diagnostics();
                let _ = self.observe_memory_sample(&process_memory);
                let (user_hit_rate, post_hit_rate) =
                    self.hydrator.get_cache().get_hit_rates().await;
                info!(
                    "Cache hit rates: users={:.2}%, posts={:.2}%",
                    user_hit_rate * 100.0,
                    post_hit_rate * 100.0
                );

                last_stats = std::time::Instant::now();
            }
        }

        if !buffer.is_empty() {
            self.process_batch(buffer).await?;
        }

        self.drain_batch_tasks(&mut batch_tasks).await?;

        error!("Jetstream stream ended unexpectedly");
        Err(TurboError::Internal("Jetstream stream ended".to_string()))
    }

    async fn spawn_batch_processing(
        &self,
        batch: Vec<JetstreamMessage>,
        batch_tasks: &mut JoinSet<TurboResult<usize>>,
    ) -> TurboResult<()> {
        let hydrator = self.hydrator.clone();
        let record_store = Arc::clone(&self.record_store);
        let event_publisher = Arc::clone(&self.event_publisher);
        let broadcast_sender = self.broadcast_sender.clone();
        let permit = self.semaphore.clone().acquire_owned().await.map_err(|e| {
            TurboError::Internal(format!("Batch semaphore closed unexpectedly: {e}"))
        })?;

        batch_tasks.spawn(async move {
            let _permit = permit;
            Self::process_batch_internal(
                hydrator,
                record_store,
                event_publisher,
                broadcast_sender,
                batch,
            )
            .await
        });

        Ok(())
    }

    pub(crate) fn resolve_batch_task_result(
        task_result: Result<TurboResult<usize>, tokio::task::JoinError>,
    ) -> TurboResult<usize> {
        match task_result {
            Ok(result) => result,
            Err(e) => Err(TurboError::TaskJoin(e)),
        }
    }

    fn handle_batch_task_result(
        &self,
        task_result: Result<TurboResult<usize>, tokio::task::JoinError>,
    ) -> TurboResult<()> {
        match Self::resolve_batch_task_result(task_result) {
            Ok(count) => {
                trace!("Processed batch of {} messages", count);
                Ok(())
            }
            Err(e) => {
                error!("Batch processing failed: {}", e);
                let mut ctx = HashMap::new();
                ctx.insert("component", "turbocharger");
                ctx.insert("operation", "batch_processing");
                self.error_reporter.capture_error(&e, ctx);
                Err(e)
            }
        }
    }

    async fn drain_batch_tasks(
        &self,
        batch_tasks: &mut JoinSet<TurboResult<usize>>,
    ) -> TurboResult<()> {
        while let Some(task_result) = batch_tasks.join_next().await {
            self.handle_batch_task_result(task_result)?;
        }

        Ok(())
    }

    async fn process_batch(&self, batch: Vec<JetstreamMessage>) -> TurboResult<usize> {
        let permit = self.semaphore.acquire().await.map_err(|e| {
            TurboError::Internal(format!("Batch semaphore closed unexpectedly: {e}"))
        })?;
        let count = Self::process_batch_internal(
            self.hydrator.clone(),
            Arc::clone(&self.record_store),
            Arc::clone(&self.event_publisher),
            self.broadcast_sender.clone(),
            batch,
        )
        .await?;
        drop(permit);
        Ok(count)
    }

    async fn process_batch_internal(
        hydrator: Hydrator<P, Po>,
        record_store: Arc<S>,
        event_publisher: Arc<E>,
        broadcast_sender: broadcast::Sender<EnrichedRecord>,
        batch: Vec<JetstreamMessage>,
    ) -> TurboResult<usize> {
        let enriched_records = hydrator.hydrate_batch(batch).await?;
        let count = enriched_records.len();

        if count == 0 {
            return Ok(0);
        }

        // Parallelize record store and event publisher operations
        let store_records = enriched_records.clone();
        let publish_records = enriched_records.clone();

        let store_future = async { record_store.store_batch(&store_records).await };

        let publish_future = async { event_publisher.publish_batch(&publish_records).await };

        // Run store and publish operations concurrently
        let (store_result, publish_result) = tokio::join!(store_future, publish_future);

        // Check results
        let _store_ids = store_result?;
        let _publish_ids = publish_result?;

        // Broadcast records (fire and forget)
        for enriched in enriched_records {
            let _ = broadcast_sender.send(enriched);
        }

        Ok(count)
    }

    fn should_process_message(&self, _message: &JetstreamMessage) -> bool {
        // Apply modulo-based sharding if specified
        // For now, just return true
        true
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EnrichedRecord> {
        self.broadcast_sender.subscribe()
    }

    fn observe_memory_sample(
        &self,
        process_memory: &ProcessMemoryDiagnostics,
    ) -> MemoryPeakDiagnostics {
        let now_unix_seconds = unix_timestamp_seconds();
        let mut peak_window = self
            .memory_peak_window
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let (Some(rss_bytes), Some(virtual_memory_bytes)) = (
            process_memory.rss_bytes,
            process_memory.virtual_memory_bytes,
        ) {
            peak_window.record(MemorySample {
                captured_at_unix_seconds: now_unix_seconds,
                rss_bytes,
                virtual_memory_bytes,
            });
        }

        peak_window.snapshot(now_unix_seconds)
    }
}

// Production-specific methods that require concrete SQLiteStore and RedisStore
impl<M, P, Po> TurboCharger<M, P, Po, SQLiteStore, RedisStore>
where
    M: MessageSource + Send + Sync + 'static,
    P: ProfileFetcher + Send + Sync + 'static,
    Po: PostFetcher + Send + Sync + 'static,
{
    pub async fn refresh_sessions(&self) -> TurboResult<()> {
        info!("Refreshing Bluesky session");

        self.bluesky_client.refresh_session_with_fallback().await?;

        info!(
            "Refreshed session credentials for {}",
            self.settings.bluesky_handle
        );
        Ok(())
    }

    pub fn start_session_refresh_task(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut refresh_interval = interval(Duration::from_secs(60 * 60));
            refresh_interval.tick().await;

            loop {
                refresh_interval.tick().await;

                if this.bluesky_client.should_refresh().await {
                    info!("Session expiring soon, refreshing proactively");
                    if let Err(e) = this.refresh_sessions().await {
                        error!("Proactive session refresh failed: {}", e);
                        let mut ctx = HashMap::new();
                        ctx.insert("component", "turbocharger");
                        ctx.insert("operation", "proactive_session_refresh");
                        this.error_reporter.capture_error(&e, ctx);
                    }
                }
            }
        });
        info!("Started session refresh task (every 1 hour)");
    }

    pub async fn get_stats(&self) -> TurboResult<TurboStats> {
        let record_count = self.sqlite_store.count_records().await?;
        let cache_metrics = self.hydrator.get_cache().get_metrics().await;
        let (user_hit_rate, post_hit_rate) = self.hydrator.get_cache().get_hit_rates().await;
        let redis_info = self.redis_store.get_stream_info().await?;

        Ok(TurboStats {
            total_records_processed: record_count,
            cache_user_hits: cache_metrics.user_hits,
            cache_user_misses: cache_metrics.user_misses,
            cache_post_hits: cache_metrics.post_hits,
            cache_post_misses: cache_metrics.post_misses,
            cache_user_hit_rate: user_hit_rate,
            cache_post_hit_rate: post_hit_rate,
            redis_stream_length: redis_info.stream_length,
            redis_version: redis_info.redis_version,
        })
    }

    pub async fn health_check(&self) -> TurboResult<HealthStatus> {
        let redis_healthy = self.redis_store.health_check().await?;
        let sqlite_available = match self.sqlite_store.count_records().await {
            Ok(_) => true,
            Err(e) => {
                error!("SQLite health check failed: {}", e);
                false
            }
        };
        let session_count = self.bluesky_client.get_session_count().await;
        let diagnostics = self
            .collect_health_diagnostics(redis_healthy, sqlite_available)
            .await;

        Ok(HealthStatus {
            healthy: derive_health(redis_healthy, sqlite_available, session_count),
            redis_connected: redis_healthy,
            sqlite_available,
            session_count,
            diagnostics,
        })
    }

    pub async fn get_runtime_diagnostics(&self) -> HealthDiagnostics {
        let redis_connected = match self.redis_store.health_check().await {
            Ok(connected) => connected,
            Err(e) => {
                error!("not_redis diagnostics health probe failed: {}", e);
                false
            }
        };

        let sqlite_available = match self.sqlite_store.count_records().await {
            Ok(_) => true,
            Err(e) => {
                error!("SQLite diagnostics availability probe failed: {}", e);
                false
            }
        };

        self.collect_health_diagnostics(redis_connected, sqlite_available)
            .await
    }

    async fn collect_health_diagnostics(
        &self,
        redis_connected: bool,
        sqlite_available: bool,
    ) -> HealthDiagnostics {
        let cache = self.hydrator.get_cache();
        let cache_metrics = cache.get_metrics().await;
        let (user_entries, post_entries) = cache.get_entry_counts();
        let (user_capacity, post_capacity) = cache.get_capacity_limits();

        let sqlite_state = match self.sqlite_store.get_state_snapshot().await {
            Ok(snapshot) => SQLiteStateDiagnostics {
                available: sqlite_available,
                db_size_bytes: Some(snapshot.db_size_bytes),
                wal_size_bytes: snapshot.wal_size_bytes,
                page_count: Some(snapshot.page_count),
                page_size_bytes: Some(snapshot.page_size_bytes),
                freelist_count: Some(snapshot.freelist_count),
                cache_size_pages: Some(snapshot.cache_size_pages),
                mmap_size_bytes: Some(snapshot.mmap_size_bytes),
                journal_mode: Some(snapshot.journal_mode),
                journal_size_limit_bytes: Some(snapshot.journal_size_limit_bytes),
                collection_error: None,
            },
            Err(e) => SQLiteStateDiagnostics {
                available: sqlite_available,
                db_size_bytes: None,
                wal_size_bytes: None,
                page_count: None,
                page_size_bytes: None,
                freelist_count: None,
                cache_size_pages: None,
                mmap_size_bytes: None,
                journal_mode: None,
                journal_size_limit_bytes: None,
                collection_error: Some(e.to_string()),
            },
        };

        let not_redis_state = match self.redis_store.get_stream_info().await {
            Ok(info) => NotRedisStateDiagnostics {
                connected: redis_connected,
                engine: info.redis_version,
                stream_name: info.stream_name,
                stream_length: Some(info.stream_length),
                configured_max_length: info.max_length,
                collection_error: None,
            },
            Err(e) => NotRedisStateDiagnostics {
                connected: redis_connected,
                engine: "not_redis".to_string(),
                stream_name: self.redis_store.get_stream_name().to_string(),
                stream_length: None,
                configured_max_length: self.redis_store.get_max_length(),
                collection_error: Some(e.to_string()),
            },
        };

        let mut process_memory = collect_process_memory_diagnostics();
        process_memory.peaks_24h = self.observe_memory_sample(&process_memory);

        HealthDiagnostics {
            process_memory,
            cache_state: CacheStateDiagnostics {
                user_entries,
                post_entries,
                user_capacity,
                post_capacity,
                user_hits: cache_metrics.user_hits,
                user_misses: cache_metrics.user_misses,
                post_hits: cache_metrics.post_hits,
                post_misses: cache_metrics.post_misses,
                total_requests: cache_metrics.total_requests,
                cache_evictions: cache_metrics.cache_evictions,
            },
            sqlite_state,
            not_redis_state,
        }
    }

    pub async fn check_and_cleanup_db(
        &self,
    ) -> TurboResult<Option<crate::storage::sqlite::CleanupResult>> {
        let max_size_bytes = (self.settings.max_db_size_mb as i64) * 1024 * 1024;
        let current_size = self.sqlite_store.get_db_size().await?;

        if current_size > max_size_bytes {
            info!(
                "Database size {}MB exceeds limit {}MB, running cleanup",
                current_size / (1024 * 1024),
                self.settings.max_db_size_mb
            );
            let result = self
                .sqlite_store
                .cleanup_with_vacuum(
                    self.settings.db_retention_days,
                    max_size_bytes,
                    self.settings.vacuum_min_bytes_freed,
                    self.settings.vacuum_min_percent_freed,
                    self.settings.cleanup_chunk_size,
                    self.settings.cleanup_chunk_delay_ms,
                )
                .await?;
            info!(
                "Cleanup complete: {} records deleted, new size: {}MB, vacuum_pending: {}",
                result.records_deleted,
                result.new_size_bytes / (1024 * 1024),
                result.vacuum_pending
            );
            return Ok(Some(result));
        }

        Ok(None)
    }

    pub fn start_db_cleanup_task(self: &Arc<Self>) {
        let this = self.clone();
        let base_interval_minutes = this.settings.cleanup_check_interval_minutes;
        let max_interval_minutes = this.settings.cleanup_backoff_max_minutes;
        let reset_skip_count = this.settings.cleanup_backoff_reset_count;

        tokio::spawn(async move {
            let mut current_interval_minutes = base_interval_minutes;
            let mut consecutive_skip_count = 0u32;

            loop {
                sleep(Duration::from_secs(current_interval_minutes * 60)).await;

                match this.check_and_cleanup_db().await {
                    Ok(Some(result)) => {
                        info!(
                            "Scheduled cleanup: {} records deleted, {}MB remaining, next check in {}min",
                            result.records_deleted,
                            result.new_size_bytes / (1024 * 1024),
                            current_interval_minutes
                        );
                        current_interval_minutes =
                            (current_interval_minutes * 2).min(max_interval_minutes);
                        consecutive_skip_count = 0;
                    }
                    Ok(None) => {
                        consecutive_skip_count += 1;
                        if consecutive_skip_count >= reset_skip_count {
                            info!(
                                "Resetting cleanup backoff: {} consecutive skips under threshold",
                                consecutive_skip_count
                            );
                            current_interval_minutes = base_interval_minutes;
                            consecutive_skip_count = 0;
                        }
                    }
                    Err(e) => {
                        error!("Database cleanup failed: {}", e);
                        current_interval_minutes =
                            (current_interval_minutes * 2).min(max_interval_minutes);
                        consecutive_skip_count = 0;
                    }
                }
            }
        });
        info!(
            "Started database cleanup task (base: {}min, max: {}min, reset after {} skips)",
            base_interval_minutes, max_interval_minutes, reset_skip_count
        );
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TurboStats {
    pub total_records_processed: i64,
    pub cache_user_hits: u64,
    pub cache_user_misses: u64,
    pub cache_post_hits: u64,
    pub cache_post_misses: u64,
    pub cache_user_hit_rate: f64,
    pub cache_post_hit_rate: f64,
    pub redis_stream_length: usize,
    pub redis_version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthStatus {
    pub healthy: bool,
    pub redis_connected: bool,
    pub sqlite_available: bool,
    pub session_count: usize,
    pub diagnostics: HealthDiagnostics,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthDiagnostics {
    pub process_memory: ProcessMemoryDiagnostics,
    pub cache_state: CacheStateDiagnostics,
    pub sqlite_state: SQLiteStateDiagnostics,
    pub not_redis_state: NotRedisStateDiagnostics,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessMemoryDiagnostics {
    pub pid: u32,
    pub rss_bytes: Option<u64>,
    pub virtual_memory_bytes: Option<u64>,
    pub source: &'static str,
    pub collection_error: Option<String>,
    pub peaks_24h: MemoryPeakDiagnostics,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryPeakDiagnostics {
    pub window_seconds: u64,
    pub samples_collected: usize,
    pub latest_sample_unix_seconds: Option<u64>,
    pub latest_sample_age_seconds: Option<u64>,
    pub rss_peak_bytes: Option<u64>,
    pub rss_peak_unix_seconds: Option<u64>,
    pub virtual_memory_peak_bytes: Option<u64>,
    pub virtual_memory_peak_unix_seconds: Option<u64>,
}

impl MemoryPeakDiagnostics {
    fn empty(window_seconds: u64) -> Self {
        Self {
            window_seconds,
            samples_collected: 0,
            latest_sample_unix_seconds: None,
            latest_sample_age_seconds: None,
            rss_peak_bytes: None,
            rss_peak_unix_seconds: None,
            virtual_memory_peak_bytes: None,
            virtual_memory_peak_unix_seconds: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheStateDiagnostics {
    pub user_entries: u64,
    pub post_entries: u64,
    pub user_capacity: usize,
    pub post_capacity: usize,
    pub user_hits: u64,
    pub user_misses: u64,
    pub post_hits: u64,
    pub post_misses: u64,
    pub total_requests: u64,
    pub cache_evictions: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SQLiteStateDiagnostics {
    pub available: bool,
    pub db_size_bytes: Option<i64>,
    pub wal_size_bytes: Option<i64>,
    pub page_count: Option<i64>,
    pub page_size_bytes: Option<i64>,
    pub freelist_count: Option<i64>,
    pub cache_size_pages: Option<i64>,
    pub mmap_size_bytes: Option<i64>,
    pub journal_mode: Option<String>,
    pub journal_size_limit_bytes: Option<i64>,
    pub collection_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NotRedisStateDiagnostics {
    pub connected: bool,
    pub engine: String,
    pub stream_name: String,
    pub stream_length: Option<usize>,
    pub configured_max_length: Option<usize>,
    pub collection_error: Option<String>,
}

/// Concrete type alias for the production TurboCharger
pub type ProductionTurboCharger =
    TurboCharger<JetstreamClient, BlueskyClient, BlueskyClient, SQLiteStore, RedisStore>;

fn derive_health(redis_connected: bool, sqlite_available: bool, session_count: usize) -> bool {
    redis_connected && sqlite_available && session_count > 0
}

fn collect_process_memory_diagnostics() -> ProcessMemoryDiagnostics {
    let pid = std::process::id();

    if let Ok(status_contents) = std::fs::read_to_string("/proc/self/status") {
        if let Some((rss_bytes, virtual_memory_bytes)) =
            parse_proc_status_memory_bytes(&status_contents)
        {
            return ProcessMemoryDiagnostics {
                pid,
                rss_bytes: Some(rss_bytes),
                virtual_memory_bytes: Some(virtual_memory_bytes),
                source: "procfs",
                collection_error: None,
                peaks_24h: MemoryPeakDiagnostics::empty(MEMORY_PEAK_WINDOW_SECS),
            };
        }
    }

    match process_memory_from_ps(pid) {
        Ok((rss_bytes, virtual_memory_bytes)) => ProcessMemoryDiagnostics {
            pid,
            rss_bytes: Some(rss_bytes),
            virtual_memory_bytes: Some(virtual_memory_bytes),
            source: "ps",
            collection_error: None,
            peaks_24h: MemoryPeakDiagnostics::empty(MEMORY_PEAK_WINDOW_SECS),
        },
        Err(error_message) => ProcessMemoryDiagnostics {
            pid,
            rss_bytes: None,
            virtual_memory_bytes: None,
            source: "unavailable",
            collection_error: Some(error_message),
            peaks_24h: MemoryPeakDiagnostics::empty(MEMORY_PEAK_WINDOW_SECS),
        },
    }
}

#[derive(Debug, Clone, Copy)]
struct MemorySample {
    captured_at_unix_seconds: u64,
    rss_bytes: u64,
    virtual_memory_bytes: u64,
}

#[derive(Debug)]
struct MemoryPeakWindow {
    window_seconds: u64,
    samples: VecDeque<MemorySample>,
}

impl MemoryPeakWindow {
    fn new(window_seconds: u64) -> Self {
        Self {
            window_seconds,
            samples: VecDeque::new(),
        }
    }

    fn record(&mut self, sample: MemorySample) {
        self.samples.push_back(sample);
        self.trim_old_samples(sample.captured_at_unix_seconds);
    }

    fn snapshot(&mut self, now_unix_seconds: u64) -> MemoryPeakDiagnostics {
        self.trim_old_samples(now_unix_seconds);

        let mut rss_peak: Option<(u64, u64)> = None;
        let mut virtual_peak: Option<(u64, u64)> = None;

        for sample in &self.samples {
            if rss_peak
                .map(|(_, peak_rss)| sample.rss_bytes > peak_rss)
                .unwrap_or(true)
            {
                rss_peak = Some((sample.captured_at_unix_seconds, sample.rss_bytes));
            }
            if virtual_peak
                .map(|(_, peak_virtual)| sample.virtual_memory_bytes > peak_virtual)
                .unwrap_or(true)
            {
                virtual_peak = Some((sample.captured_at_unix_seconds, sample.virtual_memory_bytes));
            }
        }

        let latest_sample_unix_seconds = self
            .samples
            .back()
            .map(|sample| sample.captured_at_unix_seconds);

        MemoryPeakDiagnostics {
            window_seconds: self.window_seconds,
            samples_collected: self.samples.len(),
            latest_sample_unix_seconds,
            latest_sample_age_seconds: latest_sample_unix_seconds
                .map(|captured| now_unix_seconds.saturating_sub(captured)),
            rss_peak_bytes: rss_peak.map(|(_, rss)| rss),
            rss_peak_unix_seconds: rss_peak.map(|(captured, _)| captured),
            virtual_memory_peak_bytes: virtual_peak.map(|(_, virtual_memory)| virtual_memory),
            virtual_memory_peak_unix_seconds: virtual_peak.map(|(captured, _)| captured),
        }
    }

    fn trim_old_samples(&mut self, now_unix_seconds: u64) {
        let window_start = now_unix_seconds.saturating_sub(self.window_seconds);
        while self
            .samples
            .front()
            .map(|sample| sample.captured_at_unix_seconds < window_start)
            .unwrap_or(false)
        {
            self.samples.pop_front();
        }
    }
}

fn unix_timestamp_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs()
}

fn parse_proc_status_memory_bytes(contents: &str) -> Option<(u64, u64)> {
    let mut rss_bytes = None;
    let mut virtual_memory_bytes = None;

    for line in contents.lines() {
        if rss_bytes.is_none() && line.starts_with("VmRSS:") {
            rss_bytes = parse_proc_status_kib_line(line);
        } else if virtual_memory_bytes.is_none() && line.starts_with("VmSize:") {
            virtual_memory_bytes = parse_proc_status_kib_line(line);
        }

        if rss_bytes.is_some() && virtual_memory_bytes.is_some() {
            break;
        }
    }

    match (rss_bytes, virtual_memory_bytes) {
        (Some(rss), Some(vmem)) => Some((rss, vmem)),
        _ => None,
    }
}

fn parse_proc_status_kib_line(line: &str) -> Option<u64> {
    line.split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|value| value.checked_mul(1024))
}

fn process_memory_from_ps(pid: u32) -> Result<(u64, u64), String> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-o", "vsz=", "-p", &pid.to_string()])
        .output()
        .map_err(|e| format!("failed to execute ps: {e}"))?;

    if !output.status.success() {
        return Err(format!("ps exited with status {}", output.status));
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| format!("ps output was not valid UTF-8: {e}"))?;
    parse_ps_memory_output(&stdout).ok_or_else(|| "unable to parse ps memory output".to_string())
}

fn parse_ps_memory_output(stdout: &str) -> Option<(u64, u64)> {
    let mut values = stdout
        .split_whitespace()
        .filter_map(|value| value.parse::<u64>().ok());

    let rss_bytes = values.next()?.checked_mul(1024)?;
    let virtual_memory_bytes = values.next()?.checked_mul(1024)?;
    Some((rss_bytes, virtual_memory_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_health_requires_redis_connection() {
        assert!(!derive_health(false, true, 1));
    }

    #[test]
    fn derive_health_requires_sqlite_availability() {
        assert!(!derive_health(true, false, 1));
    }

    #[test]
    fn derive_health_requires_active_sessions() {
        assert!(!derive_health(true, true, 0));
    }

    #[test]
    fn derive_health_is_true_when_all_signals_are_healthy() {
        assert!(derive_health(true, true, 1));
    }

    #[test]
    fn parse_proc_status_memory_bytes_extracts_rss_and_vmsize() {
        let contents = "\
Name:\ttest\n\
VmSize:\t  2048 kB\n\
VmRSS:\t  1024 kB\n";

        let parsed = parse_proc_status_memory_bytes(contents);
        assert_eq!(parsed, Some((1_048_576, 2_097_152)));
    }

    #[test]
    fn parse_ps_memory_output_extracts_values() {
        let parsed = parse_ps_memory_output("12345   67890\n");
        assert_eq!(parsed, Some((12_641_280, 69_519_360)));
    }

    #[test]
    fn memory_peak_window_tracks_high_watermarks_within_window() {
        let mut window = MemoryPeakWindow::new(60);
        window.record(MemorySample {
            captured_at_unix_seconds: 100,
            rss_bytes: 10,
            virtual_memory_bytes: 40,
        });
        window.record(MemorySample {
            captured_at_unix_seconds: 130,
            rss_bytes: 25,
            virtual_memory_bytes: 30,
        });

        let snapshot = window.snapshot(150);
        assert_eq!(snapshot.samples_collected, 2);
        assert_eq!(snapshot.window_seconds, 60);
        assert_eq!(snapshot.latest_sample_unix_seconds, Some(130));
        assert_eq!(snapshot.latest_sample_age_seconds, Some(20));
        assert_eq!(snapshot.rss_peak_bytes, Some(25));
        assert_eq!(snapshot.rss_peak_unix_seconds, Some(130));
        assert_eq!(snapshot.virtual_memory_peak_bytes, Some(40));
        assert_eq!(snapshot.virtual_memory_peak_unix_seconds, Some(100));
    }

    #[test]
    fn memory_peak_window_expires_old_samples() {
        let mut window = MemoryPeakWindow::new(60);
        window.record(MemorySample {
            captured_at_unix_seconds: 10,
            rss_bytes: 10,
            virtual_memory_bytes: 10,
        });
        window.record(MemorySample {
            captured_at_unix_seconds: 70,
            rss_bytes: 20,
            virtual_memory_bytes: 20,
        });
        window.record(MemorySample {
            captured_at_unix_seconds: 75,
            rss_bytes: 30,
            virtual_memory_bytes: 30,
        });

        let first_snapshot = window.snapshot(80);
        assert_eq!(first_snapshot.samples_collected, 2);
        assert_eq!(first_snapshot.rss_peak_bytes, Some(30));
        assert_eq!(first_snapshot.rss_peak_unix_seconds, Some(75));

        let second_snapshot = window.snapshot(140);
        assert_eq!(second_snapshot.samples_collected, 0);
        assert_eq!(second_snapshot.latest_sample_unix_seconds, None);
        assert_eq!(second_snapshot.rss_peak_bytes, None);
        assert_eq!(second_snapshot.virtual_memory_peak_bytes, None);
    }

    #[test]
    fn resolve_batch_task_result_propagates_worker_error() {
        let result = ProductionTurboCharger::resolve_batch_task_result(Ok(Err(
            TurboError::Internal("batch failed".to_string()),
        )));
        assert!(matches!(result, Err(TurboError::Internal(msg)) if msg == "batch failed"));
    }

    #[tokio::test]
    async fn resolve_batch_task_result_propagates_join_error() {
        let join_error = tokio::spawn(async move {
            panic!("simulated worker panic");
            #[allow(unreachable_code)]
            Ok::<usize, TurboError>(0)
        })
        .await
        .expect_err("task should panic");

        let result = ProductionTurboCharger::resolve_batch_task_result(Err(join_error));
        assert!(matches!(result, Err(TurboError::TaskJoin(_))));
    }
}
