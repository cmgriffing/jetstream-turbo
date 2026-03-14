use crate::client::{BlueskyAuthClient, BlueskyClient, JetstreamClient};
use crate::config::Settings;
use crate::hydration::{Hydrator, TurboCache};
use crate::models::enriched::EnrichedRecord;
use crate::models::{
    errors::{TurboError, TurboResult},
    jetstream::JetstreamMessage,
};
use crate::storage::{RedisStore, SQLiteStore};
use crate::telemetry::ErrorReporter;
use futures::StreamExt;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, Semaphore};
use tokio::task::JoinSet;
use tokio::time::{interval, sleep};
use tracing::{error, info, trace};

const BATCH_SIZE: usize = 25;
const MAX_WAIT_TIME_MS: u64 = 200;

pub struct TurboCharger {
    settings: Settings,
    jetstream_client: JetstreamClient,
    bluesky_client: Arc<BlueskyClient>,
    auth_client: Arc<BlueskyAuthClient>,
    hydrator: Hydrator,
    sqlite_store: Arc<SQLiteStore>,
    redis_store: Arc<RedisStore>,
    semaphore: Arc<Semaphore>,
    broadcast_sender: broadcast::Sender<EnrichedRecord>,
    error_reporter: ErrorReporter,
}

impl TurboCharger {
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
        let jetstream_client = JetstreamClient::with_defaults(settings.jetstream_hosts.clone());

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
        let hydrator = Hydrator::new(cache, bluesky_client.clone());

        // Initialize storage
        let db_path = format!("{}/jetstream.db", settings.db_dir);
        let sqlite_store = Arc::new(SQLiteStore::new(&db_path).await?);

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
            jetstream_client,
            bluesky_client,
            auth_client,
            hydrator,
            sqlite_store,
            redis_store,
            semaphore,
            broadcast_sender,
            error_reporter,
        })
    }

    pub async fn run(&self) -> TurboResult<()> {
        info!("Starting TurboCharger main loop");

        let message_stream = self.jetstream_client.stream_messages().await?;

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
        let sqlite_store = Arc::clone(&self.sqlite_store);
        let redis_store = Arc::clone(&self.redis_store);
        let broadcast_sender = self.broadcast_sender.clone();
        let permit = self.semaphore.clone().acquire_owned().await.map_err(|e| {
            TurboError::Internal(format!("Batch semaphore closed unexpectedly: {e}"))
        })?;

        batch_tasks.spawn(async move {
            let _permit = permit;
            Self::process_batch_internal(
                hydrator,
                sqlite_store,
                redis_store,
                broadcast_sender,
                batch,
            )
            .await
        });

        Ok(())
    }

    fn resolve_batch_task_result(
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
            Arc::clone(&self.sqlite_store),
            Arc::clone(&self.redis_store),
            self.broadcast_sender.clone(),
            batch,
        )
        .await?;
        drop(permit);
        Ok(count)
    }

    async fn process_batch_internal(
        hydrator: Hydrator,
        sqlite_store: Arc<SQLiteStore>,
        redis_store: Arc<RedisStore>,
        broadcast_sender: broadcast::Sender<EnrichedRecord>,
        batch: Vec<JetstreamMessage>,
    ) -> TurboResult<usize> {
        let enriched_records = hydrator.hydrate_batch(batch).await?;
        let count = enriched_records.len();

        if count == 0 {
            return Ok(0);
        }

        // Parallelize SQLite batch insert and Redis operations
        let sqlite_records = enriched_records.clone();
        let redis_records = enriched_records.clone();

        let sqlite_future = async { sqlite_store.store_batch(&sqlite_records).await };

        let redis_future = async { redis_store.publish_batch(&redis_records).await };

        // Run SQLite and Redis operations concurrently
        let (sqlite_result, redis_result) = tokio::join!(sqlite_future, redis_future);

        // Check results
        let _sqlite_ids = sqlite_result?;
        let _redis_ids = redis_result?;

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

    pub async fn refresh_sessions(&self) -> TurboResult<()> {
        info!("Refreshing Bluesky session");

        let refresh_jwt = self
            .bluesky_client
            .get_refresh_jwt()
            .await
            .ok_or_else(|| TurboError::ExpiredToken("No refresh JWT available".to_string()))?;

        let auth_response = self.auth_client.refresh_session(&refresh_jwt).await?;

        self.bluesky_client
            .refresh_sessions(
                vec![auth_response.access_jwt],
                Some(auth_response.refresh_jwt),
                auth_response.expires_at,
            )
            .await;

        info!("Refreshed session for {}", self.settings.bluesky_handle);
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

        Ok(HealthStatus {
            healthy: derive_health(redis_healthy, sqlite_available, session_count),
            redis_connected: redis_healthy,
            sqlite_available,
            session_count,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EnrichedRecord> {
        self.broadcast_sender.subscribe()
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
}

fn derive_health(redis_connected: bool, sqlite_available: bool, session_count: usize) -> bool {
    redis_connected && sqlite_available && session_count > 0
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
    fn resolve_batch_task_result_propagates_worker_error() {
        let result = TurboCharger::resolve_batch_task_result(Ok(Err(TurboError::Internal(
            "batch failed".to_string(),
        ))));
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

        let result = TurboCharger::resolve_batch_task_result(Err(join_error));
        assert!(matches!(result, Err(TurboError::TaskJoin(_))));
    }
}
