use crate::client::{
    BlueskyAuthClient, BlueskyClient, JetstreamClient, MessageSource, PostFetcher, ProfileFetcher,
};
use crate::config::{Settings, BLUESKY_API_BATCH_LIMIT};
use crate::hydration::{Hydrator, TurboCache};
use crate::models::enriched::EnrichedRecord;
use crate::models::{
    errors::{TurboError, TurboResult},
    jetstream::JetstreamMessage,
};
use crate::storage::{EventPublisher, RecordStore, RedisStore, SQLitePragmaConfig, SQLiteStore};
use crate::telemetry::ErrorReporter;
use crate::turbocharger::diagnostics::{
    derive_health, CacheStateDiagnostics, DiagnosticsCollector, HealthDiagnostics, HealthStatus,
    NotRedisStateDiagnostics, SQLiteStateDiagnostics,
};
use futures::StreamExt;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, Semaphore};
use tokio::task::JoinSet;
use tokio::time::{interval, sleep};
use tracing::{error, info, trace};

const BATCH_REPORT_LOG_TARGET: &str = "jetstream_turbo.batch_report";
// Keep Jetstream flushes aligned with Bluesky's bulk API item cap.
const BATCH_SIZE: usize = BLUESKY_API_BATCH_LIMIT;
// The hydrator can consume up to one profile batch and one post batch per flush.
// At 200ms, the time-based path can generate 5 flushes/sec, which maps to 10 API
// requests/sec in the worst case and fully consumes the shared Bluesky limit.
// 250ms keeps the timer path below that ceiling and gives partial batches a bit
// longer to fill without changing the API-imposed batch size of 25.
const MAX_WAIT_TIME_MS: u64 = 250;
const BATCH_REPORT_INTERVAL_SECS: u64 = 5 * 60;
const THROUGHPUT_HISTORY_HOURS: u64 = 48;
const VACUUM_MIN_HISTORY_HOURS: usize = 6;
const VACUUM_LOW_TRAFFIC_PERCENTILE: f64 = 0.25;

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
    diagnostics_collector: DiagnosticsCollector,
    throughput_tracker: Arc<Mutex<ThroughputTracker>>,
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
        let jetstream_client = JetstreamClient::new(
            settings.jetstream_hosts.clone(),
            settings.wanted_collections.clone(),
        )
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
        let cache = TurboCache::with_ttl(
            settings.cache_size_users,
            settings.cache_size_posts,
            Duration::from_secs(settings.cache_ttl_seconds),
        );

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

        // Initialize monitor broadcast channel
        let (broadcast_sender, _) = broadcast::channel(settings.monitor_broadcast_capacity);

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
            diagnostics_collector: DiagnosticsCollector::default(),
            throughput_tracker: Arc::new(Mutex::new(ThroughputTracker::new(
                THROUGHPUT_HISTORY_HOURS,
            ))),
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
        let mut batch_reporter = BatchReporter::new(BATCH_SIZE);
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
                                batch_reporter.record(BatchFlushReason::Full, buffer.len());
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
                        let flush_reason = if buffer.len() >= BATCH_SIZE {
                            BatchFlushReason::Full
                        } else {
                            BatchFlushReason::Timer
                        };
                        batch_reporter.record(flush_reason, buffer.len());
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
                let _ = self.diagnostics_collector.capture_memory();
                let (user_hit_rate, post_hit_rate) = self.hydrator.get_cache().get_hit_rates();
                info!(
                    "Cache hit rates: users={:.2}%, posts={:.2}%",
                    user_hit_rate * 100.0,
                    post_hit_rate * 100.0
                );
                batch_reporter.maybe_log();

                last_stats = std::time::Instant::now();
            }
        }

        if !buffer.is_empty() {
            batch_reporter.record(BatchFlushReason::Shutdown, buffer.len());
            self.process_batch(buffer).await?;
        }

        batch_reporter.log_if_window_has_data();

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
                self.record_processed_count(count as u64);
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
        self.record_processed_count(count as u64);
        drop(permit);
        Ok(count)
    }

    fn record_processed_count(&self, count: u64) {
        if count == 0 {
            return;
        }

        let mut tracker = self
            .throughput_tracker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tracker.record(count, current_unix_seconds());
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

        let store_records = enriched_records.clone();
        let publish_records = enriched_records.clone();

        let store_future = async { record_store.store_batch(&store_records).await };
        let publish_future = async { event_publisher.publish_batch(&publish_records).await };

        let (store_result, publish_result) = tokio::join!(store_future, publish_future);

        let _store_ids = store_result?;
        let _publish_ids = publish_result?;

        for enriched in enriched_records {
            let _ = broadcast_sender.send(enriched);
        }

        Ok(count)
    }

    fn should_process_message(&self, _message: &JetstreamMessage) -> bool {
        true
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EnrichedRecord> {
        self.broadcast_sender.subscribe()
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
        let cache_metrics = self.hydrator.get_cache().get_metrics();
        let (user_hit_rate, post_hit_rate) = self.hydrator.get_cache().get_hit_rates();
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
        let cache_metrics = cache.get_metrics();
        let (user_entries, post_entries) = cache.get_entry_counts();
        let (user_capacity, post_capacity) = cache.get_capacity_limits();

        let cache_state = CacheStateDiagnostics {
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
        };

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

        let process_memory = self.diagnostics_collector.capture_memory();

        DiagnosticsCollector::assemble_health(
            process_memory,
            cache_state,
            sqlite_state,
            not_redis_state,
        )
    }

    pub async fn check_and_cleanup_db(
        &self,
    ) -> TurboResult<Option<crate::storage::sqlite::CleanupResult>> {
        let max_size_bytes = (self.settings.max_db_size_mb as i64) * 1024 * 1024;
        let snapshot = self.sqlite_store.get_state_snapshot().await?;
        let current_size = snapshot.db_size_bytes;
        let reclaimable_bytes = snapshot.freelist_count * snapshot.page_size_bytes;
        let reclaimable_percent = if current_size > 0 {
            (reclaimable_bytes as f64 / current_size as f64) * 100.0
        } else {
            0.0
        };
        let vacuum_needed = reclaimable_bytes >= self.settings.vacuum_min_bytes_freed as i64
            || reclaimable_percent >= self.settings.vacuum_min_percent_freed;

        if current_size > max_size_bytes {
            let vacuum_allowed = self.is_low_throughput_vacuum_window();
            info!(
                "Database size {}MB exceeds limit {}MB, running cleanup (vacuum_allowed={})",
                current_size / (1024 * 1024),
                self.settings.max_db_size_mb,
                vacuum_allowed
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
                    vacuum_allowed,
                )
                .await?;
            info!(
                "Cleanup complete: {} records deleted, new size: {}MB, reclaimable: {}MB, vacuum_pending: {}, vacuum_deferred: {}",
                result.records_deleted,
                result.new_size_bytes / (1024 * 1024),
                result.reclaimable_bytes / (1024 * 1024),
                result.vacuum_pending,
                result.vacuum_deferred
            );
            return Ok(Some(result));
        }

        if vacuum_needed {
            let vacuum_allowed = self.is_low_throughput_vacuum_window();
            info!(
                "SQLite freelist has {}MB reclaimable ({}%), evaluating VACUUM without retention cleanup (vacuum_allowed={})",
                reclaimable_bytes / (1024 * 1024),
                reclaimable_percent as u64,
                vacuum_allowed
            );
            let result = self
                .sqlite_store
                .vacuum_reclaimable_space(
                    self.settings.vacuum_min_bytes_freed,
                    self.settings.vacuum_min_percent_freed,
                    vacuum_allowed,
                )
                .await?;
            return Ok(Some(result));
        }

        Ok(None)
    }

    fn is_low_throughput_vacuum_window(&self) -> bool {
        let mut tracker = self
            .throughput_tracker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let decision = tracker.vacuum_window_decision(
            current_unix_seconds(),
            VACUUM_MIN_HISTORY_HOURS,
            VACUUM_LOW_TRAFFIC_PERCENTILE,
        );

        info!(
            current_hour_count = decision.current_hour_count,
            completed_hour_samples = decision.completed_hour_samples,
            threshold_count = decision.threshold_count,
            enough_history = decision.enough_history,
            allow_vacuum = decision.allow_vacuum,
            "Evaluated low-throughput VACUUM window"
        );

        decision.allow_vacuum
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ThroughputHour {
    hour_start_unix_seconds: u64,
    records_processed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VacuumWindowDecision {
    allow_vacuum: bool,
    enough_history: bool,
    current_hour_count: u64,
    completed_hour_samples: usize,
    threshold_count: Option<u64>,
}

#[derive(Debug)]
struct ThroughputTracker {
    history_hours: u64,
    buckets: VecDeque<ThroughputHour>,
}

impl ThroughputTracker {
    fn new(history_hours: u64) -> Self {
        Self {
            history_hours,
            buckets: VecDeque::new(),
        }
    }

    fn record(&mut self, count: u64, now_unix_seconds: u64) {
        let hour_start = hour_start_unix_seconds(now_unix_seconds);

        match self.buckets.back_mut() {
            Some(bucket) if bucket.hour_start_unix_seconds == hour_start => {
                bucket.records_processed = bucket.records_processed.saturating_add(count);
            }
            Some(bucket) if bucket.hour_start_unix_seconds > hour_start => {
                self.insert_or_add(hour_start, count);
            }
            _ => self.buckets.push_back(ThroughputHour {
                hour_start_unix_seconds: hour_start,
                records_processed: count,
            }),
        }

        self.trim(now_unix_seconds);
    }

    fn vacuum_window_decision(
        &mut self,
        now_unix_seconds: u64,
        min_completed_hour_samples: usize,
        low_traffic_percentile: f64,
    ) -> VacuumWindowDecision {
        self.trim(now_unix_seconds);

        let current_hour_start = hour_start_unix_seconds(now_unix_seconds);
        let current_hour_count = self
            .buckets
            .iter()
            .find(|bucket| bucket.hour_start_unix_seconds == current_hour_start)
            .map(|bucket| bucket.records_processed)
            .unwrap_or(0);

        let mut completed_counts: Vec<u64> = self
            .buckets
            .iter()
            .filter(|bucket| bucket.hour_start_unix_seconds < current_hour_start)
            .map(|bucket| bucket.records_processed)
            .collect();

        let completed_hour_samples = completed_counts.len();
        let enough_history = completed_hour_samples >= min_completed_hour_samples;

        if !enough_history {
            return VacuumWindowDecision {
                allow_vacuum: current_hour_count == 0,
                enough_history,
                current_hour_count,
                completed_hour_samples,
                threshold_count: None,
            };
        }

        completed_counts.sort_unstable();
        let percentile_index = ((completed_counts.len() - 1) as f64 * low_traffic_percentile)
            .round()
            .clamp(0.0, (completed_counts.len() - 1) as f64)
            as usize;
        let threshold_count = completed_counts[percentile_index];

        VacuumWindowDecision {
            allow_vacuum: current_hour_count <= threshold_count,
            enough_history,
            current_hour_count,
            completed_hour_samples,
            threshold_count: Some(threshold_count),
        }
    }

    fn insert_or_add(&mut self, hour_start: u64, count: u64) {
        for bucket in &mut self.buckets {
            if bucket.hour_start_unix_seconds == hour_start {
                bucket.records_processed = bucket.records_processed.saturating_add(count);
                return;
            }
        }

        self.buckets.push_back(ThroughputHour {
            hour_start_unix_seconds: hour_start,
            records_processed: count,
        });
        self.buckets
            .make_contiguous()
            .sort_by_key(|bucket| bucket.hour_start_unix_seconds);
    }

    fn trim(&mut self, now_unix_seconds: u64) {
        let oldest_allowed = hour_start_unix_seconds(
            now_unix_seconds.saturating_sub(self.history_hours.saturating_mul(60 * 60)),
        );

        while self
            .buckets
            .front()
            .map(|bucket| bucket.hour_start_unix_seconds < oldest_allowed)
            .unwrap_or(false)
        {
            self.buckets.pop_front();
        }
    }
}

fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn hour_start_unix_seconds(unix_seconds: u64) -> u64 {
    unix_seconds - (unix_seconds % (60 * 60))
}

/// Concrete type alias for the production TurboCharger
pub type ProductionTurboCharger =
    TurboCharger<JetstreamClient, BlueskyClient, BlueskyClient, SQLiteStore, RedisStore>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchFlushReason {
    Full,
    Timer,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct BatchFlushSnapshot {
    total_batches: u64,
    total_messages: u64,
    average_batch_size: f64,
    average_fill_percent: f64,
    full_batches: u64,
    timer_batches: u64,
    shutdown_batches: u64,
    partial_batches: u64,
    min_batch_size: usize,
    max_batch_size: usize,
}

#[derive(Debug, Default)]
struct BatchFlushCounters {
    total_batches: u64,
    total_messages: u64,
    full_batches: u64,
    timer_batches: u64,
    shutdown_batches: u64,
    partial_batches: u64,
    min_batch_size: Option<usize>,
    max_batch_size: usize,
}

impl BatchFlushCounters {
    fn record(&mut self, batch_size_limit: usize, reason: BatchFlushReason, batch_len: usize) {
        self.total_batches += 1;
        self.total_messages += batch_len as u64;
        self.min_batch_size = Some(
            self.min_batch_size
                .map(|current| current.min(batch_len))
                .unwrap_or(batch_len),
        );
        self.max_batch_size = self.max_batch_size.max(batch_len);

        if batch_len < batch_size_limit {
            self.partial_batches += 1;
        }

        match reason {
            BatchFlushReason::Full => self.full_batches += 1,
            BatchFlushReason::Timer => self.timer_batches += 1,
            BatchFlushReason::Shutdown => self.shutdown_batches += 1,
        }
    }

    fn snapshot(&self, batch_size_limit: usize) -> Option<BatchFlushSnapshot> {
        if self.total_batches == 0 {
            return None;
        }

        let average_batch_size = self.total_messages as f64 / self.total_batches as f64;
        let average_fill_percent = (average_batch_size / batch_size_limit as f64) * 100.0;

        Some(BatchFlushSnapshot {
            total_batches: self.total_batches,
            total_messages: self.total_messages,
            average_batch_size,
            average_fill_percent,
            full_batches: self.full_batches,
            timer_batches: self.timer_batches,
            shutdown_batches: self.shutdown_batches,
            partial_batches: self.partial_batches,
            min_batch_size: self.min_batch_size.unwrap_or(0),
            max_batch_size: self.max_batch_size,
        })
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug)]
struct BatchReporter {
    batch_size_limit: usize,
    window_started_at: std::time::Instant,
    last_reported_at: std::time::Instant,
    lifetime: BatchFlushCounters,
    window: BatchFlushCounters,
}

impl BatchReporter {
    fn new(batch_size_limit: usize) -> Self {
        let now = std::time::Instant::now();
        Self {
            batch_size_limit,
            window_started_at: now,
            last_reported_at: now,
            lifetime: BatchFlushCounters::default(),
            window: BatchFlushCounters::default(),
        }
    }

    fn record(&mut self, reason: BatchFlushReason, batch_len: usize) {
        self.lifetime
            .record(self.batch_size_limit, reason, batch_len);
        self.window.record(self.batch_size_limit, reason, batch_len);
    }

    fn maybe_log(&mut self) {
        if self.last_reported_at.elapsed() < Duration::from_secs(BATCH_REPORT_INTERVAL_SECS) {
            return;
        }

        self.log_if_window_has_data();
    }

    fn log_if_window_has_data(&mut self) {
        let Some(window) = self.window.snapshot(self.batch_size_limit) else {
            self.last_reported_at = std::time::Instant::now();
            self.window_started_at = self.last_reported_at;
            return;
        };

        let lifetime = self
            .lifetime
            .snapshot(self.batch_size_limit)
            .expect("lifetime counters must exist when window counters exist");
        let window_elapsed = self.window_started_at.elapsed().as_secs();

        info!(
            target: BATCH_REPORT_LOG_TARGET,
            report_window_seconds = window_elapsed,
            batch_size_limit = self.batch_size_limit,
            window_batches = window.total_batches,
            window_messages = window.total_messages,
            window_avg_batch_size = format_args!("{:.2}", window.average_batch_size),
            window_avg_fill_percent = format_args!("{:.1}", window.average_fill_percent),
            window_full_batches = window.full_batches,
            window_timer_batches = window.timer_batches,
            window_shutdown_batches = window.shutdown_batches,
            window_partial_batches = window.partial_batches,
            window_min_batch_size = window.min_batch_size,
            window_max_batch_size = window.max_batch_size,
            lifetime_batches = lifetime.total_batches,
            lifetime_messages = lifetime.total_messages,
            lifetime_avg_batch_size = format_args!("{:.2}", lifetime.average_batch_size),
            lifetime_avg_fill_percent = format_args!("{:.1}", lifetime.average_fill_percent),
            lifetime_full_batches = lifetime.full_batches,
            lifetime_timer_batches = lifetime.timer_batches,
            lifetime_shutdown_batches = lifetime.shutdown_batches,
            lifetime_partial_batches = lifetime.partial_batches,
            "Jetstream batch flush report"
        );

        self.window.reset();
        self.last_reported_at = std::time::Instant::now();
        self.window_started_at = self.last_reported_at;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn batch_flush_counters_capture_mix_of_full_and_partial_batches() {
        const TEST_BATCH_SIZE: usize = 25;
        let mut counters = BatchFlushCounters::default();

        counters.record(TEST_BATCH_SIZE, BatchFlushReason::Full, TEST_BATCH_SIZE);
        counters.record(TEST_BATCH_SIZE, BatchFlushReason::Timer, 12);
        counters.record(TEST_BATCH_SIZE, BatchFlushReason::Shutdown, 3);

        let snapshot = counters
            .snapshot(TEST_BATCH_SIZE)
            .expect("snapshot should exist");
        assert_eq!(snapshot.total_batches, 3);
        assert_eq!(snapshot.total_messages, 40);
        assert_eq!(snapshot.full_batches, 1);
        assert_eq!(snapshot.timer_batches, 1);
        assert_eq!(snapshot.shutdown_batches, 1);
        assert_eq!(snapshot.partial_batches, 2);
        assert_eq!(snapshot.min_batch_size, 3);
        assert_eq!(snapshot.max_batch_size, 25);
        assert!((snapshot.average_batch_size - 13.33).abs() < 0.01);
        assert!((snapshot.average_fill_percent - 53.33).abs() < 0.01);
    }

    #[test]
    fn batch_flush_counters_reset_clears_window_data() {
        const TEST_BATCH_SIZE: usize = 25;
        let mut counters = BatchFlushCounters::default();
        counters.record(TEST_BATCH_SIZE, BatchFlushReason::Timer, 7);

        assert!(counters.snapshot(TEST_BATCH_SIZE).is_some());
        counters.reset();
        assert!(counters.snapshot(TEST_BATCH_SIZE).is_none());
    }

    #[test]
    fn throughput_tracker_allows_vacuum_during_low_traffic_hours() {
        let mut tracker = ThroughputTracker::new(48);
        let base = 1_800_000_000;

        for hour in 0..8 {
            tracker.record(10_000 + hour, base + hour * 60 * 60);
        }
        tracker.record(500, base + 8 * 60 * 60);

        let decision = tracker.vacuum_window_decision(base + 8 * 60 * 60 + 60, 6, 0.25);

        assert!(decision.enough_history);
        assert!(decision.allow_vacuum);
        assert_eq!(decision.completed_hour_samples, 8);
        assert_eq!(decision.current_hour_count, 500);
    }

    #[test]
    fn throughput_tracker_defers_vacuum_during_peak_hours() {
        let mut tracker = ThroughputTracker::new(48);
        let base = 1_800_000_000;

        for hour in 0..8 {
            tracker.record(1_000 + hour, base + hour * 60 * 60);
        }
        tracker.record(20_000, base + 8 * 60 * 60);

        let decision = tracker.vacuum_window_decision(base + 8 * 60 * 60 + 60, 6, 0.25);

        assert!(decision.enough_history);
        assert!(!decision.allow_vacuum);
        assert_eq!(decision.current_hour_count, 20_000);
    }

    #[test]
    fn throughput_tracker_retains_only_configured_history_window() {
        let mut tracker = ThroughputTracker::new(48);
        let base = 1_800_000_000;

        for hour in 0..50 {
            tracker.record(100, base + hour * 60 * 60);
        }

        assert_eq!(tracker.buckets.len(), 49);
        assert_eq!(
            tracker.buckets.front().unwrap().hour_start_unix_seconds,
            hour_start_unix_seconds(base + 60 * 60)
        );
    }
}
