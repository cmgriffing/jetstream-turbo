use crate::client::{
    BlueskyAuthClient, BlueskyClient, ContainmentPolicy, JetstreamClient, MessageSource,
    PostFetcher, ProfileFetcher, RequestRetryPolicy,
};
use crate::config::Settings;
use crate::hydration::{Hydrator, TurboCache};
use crate::models::enriched::EnrichedRecord;
use crate::models::{
    errors::{TurboError, TurboResult},
    jetstream::JetstreamMessage,
    recovery::{IngressBatch, IngressEvent, IngressRange},
};
use crate::storage::{EventPublisher, RecordStore, RedisStore, SQLitePragmaConfig, SQLiteStore};
use crate::telemetry::ErrorReporter;
use crate::turbocharger::coordinator::CompletionFrontier;
use crate::turbocharger::diagnostics::{
    derive_health, CacheStateDiagnostics, DiagnosticsCollector, HealthDiagnostics, HealthStatus,
    NotRedisStateDiagnostics, ReadinessDiagnostics, SQLiteStateDiagnostics,
};
use crate::turbocharger::progress::PipelineStage;
use crate::turbocharger::progress::{BatchLifecycle, PipelineProgress};
use crate::turbocharger::progress::{
    PipelineProgressSnapshot, PipelineReadinessState, ProgressThresholds,
};
use crate::turbocharger::{FailureSupervisor, RecoveryDecision};
use chrono::{DateTime, Timelike, Utc};
use futures::StreamExt;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio::time::{interval, sleep, timeout_at, Instant};
use tracing::{error, info, trace, warn};

const BATCH_SIZE: usize = 25;
const BATCH_REPORT_LOG_TARGET: &str = "jetstream_turbo.batch_report";
// The hydrator can consume up to one profile batch and one post batch per flush.
// At 200ms, the time-based path can generate 5 flushes/sec, which maps to 10 API
// requests/sec in the worst case and fully consumes the shared Bluesky limit.
// 250ms keeps the timer path below that ceiling and gives partial batches a bit
// longer to fill without changing the API-imposed batch size of 25.
const MAX_WAIT_TIME_MS: u64 = 250;
const BATCH_REPORT_INTERVAL_SECS: u64 = 5 * 60;

pub type RunResult<T> = Result<T, RunFailure>;

/// Internal run-loop failure context retaining the portable range of failed batch work.
#[derive(Debug)]
pub struct RunFailure {
    error: Box<TurboError>,
    failed_range: Option<IngressRange>,
}

impl RunFailure {
    fn batch(error: TurboError, failed_range: IngressRange) -> Self {
        Self {
            error: Box::new(error),
            failed_range: Some(failed_range),
        }
    }

    pub fn error(&self) -> &TurboError {
        self.error.as_ref()
    }

    pub fn failed_range(&self) -> Option<&IngressRange> {
        self.failed_range.as_ref()
    }
}

impl From<TurboError> for RunFailure {
    fn from(error: TurboError) -> Self {
        Self {
            error: Box::new(error),
            failed_range: None,
        }
    }
}

impl fmt::Display for RunFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for RunFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

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
    progress: Arc<PipelineProgress>,
    completion_frontier: Arc<Mutex<CompletionFrontier>>,
    failure_supervisor: Arc<FailureSupervisor>,
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
        let progress = Arc::new(PipelineProgress::new(
            settings.max_concurrent_requests,
            settings.channel_capacity,
        ));
        let jetstream_client = JetstreamClient::new(
            settings.jetstream_hosts.clone(),
            settings.wanted_collections.clone(),
        )
        .with_channel_capacity(settings.channel_capacity)
        .with_endpoint_backoff(
            Duration::from_secs(settings.jetstream_endpoint_backoff_min_secs),
            Duration::from_secs(settings.jetstream_endpoint_backoff_max_secs),
        )
        .with_cursor_overlap(Duration::from_secs(settings.jetstream_cursor_overlap_secs))
        .with_progress_tracker(Arc::clone(&progress));
        let jetstream_client = if settings.jetstream_recovery_deadlines_enabled {
            jetstream_client
                .with_connection_timeout(Duration::from_secs(
                    settings.jetstream_connect_timeout_secs,
                ))
                .with_data_idle_timeout(Duration::from_secs(
                    settings.jetstream_data_idle_timeout_secs,
                ))
        } else {
            jetstream_client
                .without_connection_timeout()
                .without_data_idle_timeout()
        };

        // Authenticate directly with Bluesky
        let auth_client = Arc::new(BlueskyAuthClient::with_api_url(
            settings.bluesky_handle.clone(),
            settings.bluesky_app_password.clone(),
            settings.bluesky_api_url.clone(),
        )?);

        let auth_response = auth_client.authenticate().await?;
        info!(
            "Successfully authenticated with Bluesky as {}",
            settings.bluesky_handle
        );
        let containment_policy = ContainmentPolicy {
            min_delay: settings.recovery_min_delay,
            max_delay: settings.recovery_max_delay,
            persistence_threshold: settings.recovery_persistence_threshold,
            isolation_request_budget: settings.isolation_request_budget,
        };
        let bluesky_client = Arc::new(BlueskyClient::new_with_policies(
            vec![auth_response.access_jwt.clone()],
            Some(auth_client.clone()),
            settings.profile_batch_size,
            settings.post_batch_size,
            settings.profile_batch_wait_ms,
            settings.post_batch_wait_ms,
            RequestRetryPolicy {
                max_retries: settings.max_retries,
                base_delay: settings.retry_base_delay,
                max_delay: settings.retry_max_delay,
            },
            containment_policy,
        )?);
        bluesky_client
            .refresh_sessions(
                vec![auth_response.access_jwt],
                Some(auth_response.refresh_jwt),
                auth_response.expires_at,
            )
            .await;

        // Initialize cache
        let cache = TurboCache::new_with_negative_cache(
            settings.cache_size_users,
            settings.cache_size_posts,
            settings.negative_post_cache_capacity,
            settings.negative_post_cache_ttl,
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
        let checkpoint = sqlite_store.load_ingestion_checkpoint().await?;
        let completion_frontier =
            Arc::new(Mutex::new(CompletionFrontier::new(checkpoint.as_ref())));
        let jetstream_client = if settings.jetstream_cursor_replay_enabled {
            jetstream_client.with_checkpoint_store(Arc::clone(&sqlite_store))
        } else {
            jetstream_client
        };

        let redis_store = Arc::new(
            RedisStore::new(
                &settings.redis_url,
                settings.stream_name_redis.clone(),
                settings.trim_maxlen,
            )
            .await?,
        );

        // Initialize semaphore for concurrency control
        let semaphore = Arc::new(Semaphore::new(settings.max_concurrent_requests.max(1)));

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
            progress,
            completion_frontier,
            failure_supervisor: Arc::new(FailureSupervisor::new(containment_policy)),
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
    pub async fn run(&self) -> RunResult<()> {
        info!("Starting TurboCharger main loop");

        let message_stream = self.message_source.stream_messages().await?;

        let mut last_stats = std::time::Instant::now();
        let mut batch_reporter = BatchReporter::new(BATCH_SIZE);
        let checkpoint = self.sqlite_store.load_ingestion_checkpoint().await?;
        let mut next_ingress_ordinal = checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.ingress_ordinal.saturating_add(1))
            .unwrap_or(1);
        let mut buffer: Vec<IngressEvent> = Vec::with_capacity(BATCH_SIZE);
        let mut flush_interval = interval(Duration::from_millis(MAX_WAIT_TIME_MS));
        let mut batch_buffer: Vec<IngressEvent> = Vec::with_capacity(BATCH_SIZE);
        let mut batch_tasks: JoinSet<RunResult<BatchCompletion>> = JoinSet::new();

        tokio::pin!(message_stream);

        loop {
            tokio::select! {
                result = message_stream.next() => {
                    match result {
                        Some(Ok(message)) => {
                            if self.should_process_message(&message) {
                                if let Some(event) = Self::accept_ingress_event(
                                    self.progress.as_ref(),
                                    &mut next_ingress_ordinal,
                                    message,
                                ) {
                                    buffer.push(event);
                                }
                            }

                            if buffer.len() >= BATCH_SIZE {
                                batch_reporter.record(BatchFlushReason::Full, buffer.len());
                                batch_buffer.clear();
                                batch_buffer.append(&mut buffer);
                                self.spawn_batch_processing(
                                    ingress_batch(std::mem::take(&mut batch_buffer))?,
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
                        batch_buffer.append(&mut buffer);
                        self.spawn_batch_processing(
                            ingress_batch(std::mem::take(&mut batch_buffer))?,
                            &mut batch_tasks,
                        )
                        .await?;
                    }
                }
            }

            while let Some(task_result) = batch_tasks.try_join_next() {
                if let Err(error) = self.handle_batch_task_result(task_result).await {
                    Self::abort_and_drain_batch_tasks(&mut batch_tasks).await;
                    return Err(error);
                }
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
                let snapshot = self.progress.snapshot(self.progress_thresholds());
                info!(
                    recovery_phase = ?snapshot.recovery_phase,
                    active_endpoint = ?snapshot.connected_endpoint,
                    reconnect_reason = ?snapshot.last_reconnect_reason,
                    last_received_event_time_us = ?snapshot.last_received_event_time_us,
                    last_committed_event_time_us = ?snapshot.last_committed_event_time_us,
                    received_lag_us = ?snapshot.received_lag_us,
                    committed_lag_us = ?snapshot.committed_lag_us,
                    replayed_events = snapshot.replayed_events,
                    duplicate_events = snapshot.duplicate_events,
                    blocked_send_duration_ms = snapshot.blocked_send_duration_ms,
                    readiness_state = ?snapshot.readiness_state,
                    stale_stage = ?snapshot.stale_stage,
                    ingress_age_seconds = ?snapshot.ingress_age_seconds,
                    completion_age_seconds = ?snapshot.completion_age_seconds,
                    active_permits = snapshot.active_permits,
                    maximum_permits = snapshot.maximum_permits,
                    input_occupancy = snapshot.input_occupancy,
                    input_drops = snapshot.input_drops,
                    "Pipeline progress summary"
                );

                last_stats = std::time::Instant::now();
            }
        }

        if !buffer.is_empty() {
            batch_reporter.record(BatchFlushReason::Shutdown, buffer.len());
            let completion = self.process_batch(ingress_batch(buffer)?).await?;
            self.persist_batch_completion(completion).await?;
        }

        batch_reporter.log_if_window_has_data();

        self.drain_batch_tasks(&mut batch_tasks).await?;

        error!("Jetstream stream ended unexpectedly");
        Err(TurboError::Internal("Jetstream stream ended".to_string()).into())
    }

    pub async fn record_run_failure(&self, failure: &RunFailure) -> RecoveryDecision {
        let durable_checkpoint_ordinal = self
            .completion_frontier
            .lock()
            .await
            .durable_checkpoint_ordinal();
        let decision = self.failure_supervisor.record_failure(
            failure.error(),
            failure.failed_range(),
            durable_checkpoint_ordinal,
        );
        self.bluesky_client
            .set_failure_recurrence(decision.recurrence);
        let snapshot = self.failure_supervisor.snapshot();
        metrics::gauge!("pipeline_failure_recurrence").set(decision.recurrence as f64);
        metrics::gauge!("pipeline_recovery_delay_seconds").set(decision.delay.as_secs_f64());
        metrics::gauge!("pipeline_failure_persistent").set(if decision.persistent {
            1.0
        } else {
            0.0
        });
        if let (Some(subtype), Some(stage)) = (snapshot.subtype, snapshot.stage) {
            metrics::counter!(
                "pipeline_failures_total",
                "subtype" => subtype.as_str(),
                "stage" => stage.as_str(),
                "boundary" => if snapshot.boundary_present { "present" } else { "absent" }
            )
            .increment(1);
        }
        if decision.log_terminal {
            error!(
                fingerprint = snapshot.fingerprint.as_deref(),
                operation = snapshot.operation.as_deref(),
                category = snapshot.category.as_deref(),
                recurrence = decision.recurrence,
                persistent = decision.persistent,
                retryable = decision.retryable,
                recovery_delay_ms = decision.delay.as_millis(),
                isolation = ?snapshot.isolation,
                failure_subtype = snapshot.subtype.map(|value| value.as_str()),
                failure_stage = snapshot.stage.map(|value| value.as_str()),
                boundary_present = snapshot.boundary_present,
                incident_start_checkpoint_ordinal = snapshot.incident_start_checkpoint_ordinal,
                "TurboCharger run failure entered containment"
            );
        } else {
            warn!(
                fingerprint = snapshot.fingerprint.as_deref(),
                recurrence = decision.recurrence,
                recovery_delay_ms = decision.delay.as_millis(),
                "Repeated TurboCharger failure remains contained"
            );
        }
        decision
    }

    pub fn minimum_recovery_delay(&self) -> Duration {
        self.settings.recovery_min_delay
    }

    async fn spawn_batch_processing(
        &self,
        batch: IngressBatch,
        batch_tasks: &mut JoinSet<RunResult<BatchCompletion>>,
    ) -> TurboResult<()> {
        let hydrator = self.hydrator.clone();
        let record_store = Arc::clone(&self.record_store);
        let event_publisher = Arc::clone(&self.event_publisher);
        let broadcast_sender = self.broadcast_sender.clone();
        let progress = Arc::clone(&self.progress);
        let batch_id = progress.batch_started();
        let timeout = self
            .settings
            .pipeline_deadlines_enabled
            .then(|| Duration::from_secs(self.settings.batch_execution_timeout_secs));
        let permit = self.semaphore.clone().acquire_owned().await.map_err(|e| {
            TurboError::Internal(format!("Batch semaphore closed unexpectedly: {e}"))
        })?;
        progress.batch_running(batch_id);

        let failed_range = batch.range().clone();
        batch_tasks.spawn(async move {
            let _permit = permit;
            Self::process_batch_internal(
                hydrator,
                record_store,
                event_publisher,
                broadcast_sender,
                batch,
                progress,
                batch_id,
                timeout,
            )
            .await
            .map_err(|error| RunFailure::batch(error, failed_range))
        });

        Ok(())
    }

    pub(crate) fn resolve_batch_task_result(
        task_result: Result<RunResult<BatchCompletion>, tokio::task::JoinError>,
    ) -> RunResult<BatchCompletion> {
        match task_result {
            Ok(result) => result,
            Err(e) => {
                metrics::counter!("pipeline_batch_join_failures_total").increment(1);
                Err(TurboError::TaskJoin(Box::new(e)).into())
            }
        }
    }

    async fn handle_batch_task_result(
        &self,
        task_result: Result<RunResult<BatchCompletion>, tokio::task::JoinError>,
    ) -> RunResult<()> {
        match Self::resolve_batch_task_result(task_result) {
            Ok(completion) => {
                trace!(
                    "Processed batch of {} messages through ingress ordinal {}",
                    completion.processed_count,
                    completion.range.end_ordinal
                );
                self.persist_batch_completion(completion).await?;
                Ok(())
            }
            Err(failure) => {
                error!("Batch processing failed: {}", failure);
                let mut ctx = HashMap::new();
                ctx.insert("component", "turbocharger");
                ctx.insert("operation", "batch_processing");
                self.error_reporter.capture_error(failure.error(), ctx);
                Err(failure)
            }
        }
    }

    async fn drain_batch_tasks(
        &self,
        batch_tasks: &mut JoinSet<RunResult<BatchCompletion>>,
    ) -> RunResult<()> {
        while let Some(task_result) = batch_tasks.join_next().await {
            self.handle_batch_task_result(task_result).await?;
        }

        Ok(())
    }

    async fn abort_and_drain_batch_tasks(batch_tasks: &mut JoinSet<RunResult<BatchCompletion>>) {
        batch_tasks.abort_all();
        while batch_tasks.join_next().await.is_some() {}
    }

    async fn process_batch(&self, batch: IngressBatch) -> RunResult<BatchCompletion> {
        let permit = self.semaphore.acquire().await.map_err(|e| {
            TurboError::Internal(format!("Batch semaphore closed unexpectedly: {e}"))
        })?;
        let batch_id = self.progress.batch_started();
        self.progress.batch_running(batch_id);
        let failed_range = batch.range().clone();
        let count = Self::process_batch_internal(
            self.hydrator.clone(),
            Arc::clone(&self.record_store),
            Arc::clone(&self.event_publisher),
            self.broadcast_sender.clone(),
            batch,
            Arc::clone(&self.progress),
            batch_id,
            self.settings
                .pipeline_deadlines_enabled
                .then(|| Duration::from_secs(self.settings.batch_execution_timeout_secs)),
        )
        .await
        .map_err(|error| RunFailure::batch(error, failed_range))?;
        drop(permit);
        Ok(count)
    }

    async fn persist_batch_completion(&self, completion: BatchCompletion) -> TurboResult<()> {
        if let Some(checkpoint) = persist_batch_completion(
            self.completion_frontier.as_ref(),
            self.sqlite_store.as_ref(),
            completion,
        )
        .await?
        {
            if let Some(recovered) = self.failure_supervisor.observe_checkpoint(&checkpoint) {
                self.bluesky_client.set_failure_recurrence(0);
                metrics::gauge!("pipeline_failure_recurrence").set(0.0);
                metrics::gauge!("pipeline_failure_persistent").set(0.0);
                info!(
                    fingerprint = recovered.fingerprint.as_deref(),
                    final_recurrence = recovered.recurrence,
                    duration_ms = recovered
                        .first_occurrence_unix_ms
                        .zip(recovered.last_occurrence_unix_ms)
                        .map(|(first, last)| last.saturating_sub(first)),
                    "Durable checkpoint progress cleared failure containment"
                );
            }
            let became_live = self.progress.checkpoint_committed(
                checkpoint.cursor.time_us,
                Duration::from_secs(self.settings.jetstream_committed_lag_threshold_secs),
                self.settings.jetstream_live_stability_observations,
            );
            let now_us = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros()
                .min(u64::MAX as u128) as u64;
            let committed_lag_us = now_us.saturating_sub(checkpoint.cursor.time_us);
            metrics::gauge!("jetstream_last_committed_event_time_us")
                .set(checkpoint.cursor.time_us as f64);
            metrics::gauge!("jetstream_committed_lag_seconds")
                .set(committed_lag_us as f64 / 1_000_000.0);
            let snapshot = self.progress.snapshot(self.progress_thresholds());
            metrics::gauge!("jetstream_recovery_phase")
                .set(recovery_phase_code(snapshot.recovery_phase));
            info!(
                recovery_phase = ?snapshot.recovery_phase,
                committed_event_time_us = checkpoint.cursor.time_us,
                committed_lag_us,
                stable_observations = snapshot.live_stability_observations,
                "Durable Jetstream checkpoint advanced"
            );
            if became_live {
                if let Some(recovery_duration_ms) = snapshot.recovery_duration_ms {
                    metrics::histogram!("jetstream_recovery_duration_seconds")
                        .record(recovery_duration_ms as f64 / 1_000.0);
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_batch_internal(
        hydrator: Hydrator<P, Po>,
        record_store: Arc<S>,
        event_publisher: Arc<E>,
        broadcast_sender: broadcast::Sender<EnrichedRecord>,
        batch: IngressBatch,
        progress: Arc<PipelineProgress>,
        batch_id: u64,
        timeout: Option<Duration>,
    ) -> TurboResult<BatchCompletion> {
        let lifecycle = BatchLifecycle::new(Arc::clone(&progress), batch_id);
        let (events, range) = batch.into_parts();
        let deadline = timeout
            .map(|duration| Instant::now() + duration)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(100 * 365 * 24 * 60 * 60));
        let timeout_secs = timeout.map(|duration| duration.as_secs()).unwrap_or(0);
        let source_event_ids = events
            .iter()
            .map(|event| event.cursor.source_event_id.clone())
            .collect::<Vec<_>>();
        let completed_source_event_ids = match record_store
            .completed_source_event_ids(&source_event_ids)
            .await
        {
            Ok(completed) => completed,
            Err(error) => {
                lifecycle.failed();
                return Err(error);
            }
        };
        let duplicate_count = completed_source_event_ids.len();
        if duplicate_count > 0 {
            metrics::counter!("jetstream_replay_duplicates_total")
                .increment(duplicate_count as u64);
            progress.duplicate_events(duplicate_count);
        }
        let messages = events
            .into_iter()
            .filter(|event| !completed_source_event_ids.contains(&event.cursor.source_event_id))
            .map(|event| event.message)
            .collect();
        progress.batch_stage(batch_id, PipelineStage::Hydration);
        let enriched_records = match timeout_at(deadline, hydrator.hydrate_batch(messages)).await {
            Ok(Ok(records)) => records,
            Ok(Err(error)) => {
                lifecycle.failed();
                return Err(error);
            }
            Err(_) => {
                let error = batch_timeout(batch_id, PipelineStage::Hydration, timeout_secs);
                lifecycle.timed_out();
                return Err(error);
            }
        };
        let count = enriched_records.len();

        if count == 0 {
            lifecycle.completed(0);
            return Ok(BatchCompletion {
                processed_count: 0,
                range,
            });
        }

        progress.batch_stage(batch_id, PipelineStage::Storage);
        match timeout_at(deadline, record_store.store_batch(&enriched_records)).await {
            Ok(Ok(_)) => progress.store_succeeded(),
            Ok(Err(error)) => {
                lifecycle.failed();
                return Err(error);
            }
            Err(_) => {
                let error = batch_timeout(batch_id, PipelineStage::Storage, timeout_secs);
                lifecycle.timed_out();
                return Err(error);
            }
        }

        progress.batch_stage(batch_id, PipelineStage::Publication);
        match timeout_at(deadline, event_publisher.publish_batch(&enriched_records)).await {
            Ok(Ok(_)) => progress.publication_succeeded(),
            Ok(Err(error)) => {
                lifecycle.failed();
                return Err(error);
            }
            Err(_) => {
                let error = batch_timeout(batch_id, PipelineStage::Publication, timeout_secs);
                lifecycle.timed_out();
                return Err(error);
            }
        }

        progress.batch_stage(batch_id, PipelineStage::Broadcast);
        let receivers = broadcast_sender.receiver_count();
        let mut successful_sends = 0;
        for enriched in enriched_records {
            if broadcast_sender.send(enriched).is_ok() {
                successful_sends += 1;
            }
        }
        progress.broadcast_state(receivers, successful_sends);
        lifecycle.completed(count);

        Ok(BatchCompletion {
            processed_count: count,
            range,
        })
    }

    fn should_process_message(&self, _message: &JetstreamMessage) -> bool {
        true
    }

    fn accept_ingress_event(
        progress: &PipelineProgress,
        next_ingress_ordinal: &mut u64,
        message: JetstreamMessage,
    ) -> Option<IngressEvent> {
        let message_kind = message.kind;
        let Some(event) = IngressEvent::new(*next_ingress_ordinal, message) else {
            let rejected_count = progress.rejected_cursorless_ingress(message_kind);
            metrics::counter!(
                "jetstream_ingress_rejections_total",
                "reason" => "missing_time_us",
                "kind" => message_kind.as_str()
            )
            .increment(1);
            if rejected_count == 1 || rejected_count.is_power_of_two() {
                warn!(
                    reason = "missing_time_us",
                    kind = message_kind.as_str(),
                    rejected_count,
                    "Rejected cursorless Jetstream ingress event"
                );
            }
            return None;
        };

        *next_ingress_ordinal = next_ingress_ordinal.saturating_add(1);
        Some(event)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EnrichedRecord> {
        self.broadcast_sender.subscribe()
    }

    fn progress_thresholds(&self) -> ProgressThresholds {
        ProgressThresholds {
            startup_grace: Duration::from_secs(self.settings.pipeline_startup_grace_secs),
            ingress_idle: Duration::from_secs(self.settings.jetstream_data_idle_timeout_secs),
            batch_execution: Duration::from_secs(self.settings.batch_execution_timeout_secs),
            recovery_successes: self.settings.readiness_recovery_successes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BatchCompletion {
    processed_count: usize,
    range: IngressRange,
}

fn ingress_batch(events: Vec<IngressEvent>) -> TurboResult<IngressBatch> {
    IngressBatch::new(events).ok_or_else(|| {
        TurboError::InvalidMessage("ingress batch must be non-empty and ordered".to_string())
    })
}

fn recovery_phase_code(phase: crate::models::recovery::RecoveryPhase) -> f64 {
    match phase {
        crate::models::recovery::RecoveryPhase::Connecting => 0.0,
        crate::models::recovery::RecoveryPhase::Replaying => 1.0,
        crate::models::recovery::RecoveryPhase::CatchingUp => 2.0,
        crate::models::recovery::RecoveryPhase::Live => 3.0,
        crate::models::recovery::RecoveryPhase::UnrecoverableGap => 4.0,
    }
}

/// Whether `current_hour` (UTC) falls inside the configured low-traffic
/// window `[start_hour, end_hour)`, handling windows that wrap past midnight
/// (e.g. 22:00-02:00).
fn in_vacuum_window(current_hour: u32, start_hour: u32, end_hour: u32) -> bool {
    if start_hour < end_hour {
        current_hour >= start_hour && current_hour < end_hour
    } else {
        current_hour >= start_hour || current_hour < end_hour
    }
}

/// Scheduling decision for a pending VACUUM: run when the current UTC hour is
/// inside the window, or when the pending age exceeds `max_defer_hours`.
fn vacuum_should_run_now(
    now: DateTime<Utc>,
    pending_since: Option<DateTime<Utc>>,
    window_start_hour: u32,
    window_end_hour: u32,
    max_defer_hours: u64,
) -> bool {
    if in_vacuum_window(now.hour(), window_start_hour, window_end_hour) {
        return true;
    }
    pending_since
        .map(|since| now.signed_duration_since(since).num_hours() >= max_defer_hours as i64)
        .unwrap_or(false)
}

async fn persist_batch_completion(
    frontier: &Mutex<CompletionFrontier>,
    sqlite_store: &SQLiteStore,
    completion: BatchCompletion,
) -> TurboResult<Option<crate::models::recovery::IngestionCheckpoint>> {
    let mut frontier = frontier.lock().await;
    let mut staged_frontier = frontier.clone();
    let Some(checkpoint) = staged_frontier.record_completed(completion.range)? else {
        *frontier = staged_frontier;
        return Ok(None);
    };
    sqlite_store
        .advance_ingestion_checkpoint(&checkpoint)
        .await?;
    *frontier = staged_frontier;
    Ok(Some(checkpoint))
}

fn batch_timeout(batch_id: u64, stage: PipelineStage, timeout_secs: u64) -> TurboError {
    metrics::counter!("pipeline_batch_timeouts_total", "stage" => format!("{stage:?}").to_lowercase()).increment(1);
    TurboError::BatchStageTimeout {
        batch_id,
        stage: format!("{stage:?}").to_lowercase(),
        timeout_secs,
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
        let pipeline_progress = self.progress.snapshot(self.progress_thresholds());
        let diagnostics = self
            .collect_health_diagnostics(redis_healthy, sqlite_available, pipeline_progress.clone())
            .await;
        let containment = diagnostics.failure_containment.clone();

        let dependency_healthy = redis_healthy && sqlite_available && session_count > 0;
        let transport_connected = pipeline_progress.connected_endpoint.is_some();
        let recovery_phase = pipeline_progress.recovery_phase;
        let unrecoverable_gap = pipeline_progress.unrecoverable_gap.clone();
        let readiness = if unrecoverable_gap.is_some() {
            ReadinessDiagnostics {
                state: PipelineReadinessState::Stale,
                stage: Some(PipelineStage::Ingress),
                reason: Some("unrecoverable_cursor_gap".to_string()),
                transport_connected,
                recovery_phase,
                unrecoverable_gap,
            }
        } else if pipeline_progress.input_drops > 0 {
            ReadinessDiagnostics {
                state: PipelineReadinessState::Stale,
                stage: Some(PipelineStage::Ingress),
                reason: Some("input_drop_correctness_failure".to_string()),
                transport_connected,
                recovery_phase,
                unrecoverable_gap: None,
            }
        } else if containment.persistent {
            ReadinessDiagnostics {
                state: PipelineReadinessState::Stale,
                stage: Some(PipelineStage::Hydration),
                reason: Some(format!(
                    "persistent_{}",
                    containment
                        .category
                        .as_deref()
                        .unwrap_or("upstream_failure")
                )),
                transport_connected,
                recovery_phase,
                unrecoverable_gap: None,
            }
        } else if !dependency_healthy {
            ReadinessDiagnostics {
                state: PipelineReadinessState::Stale,
                stage: None,
                reason: Some("dependency_unhealthy".to_string()),
                transport_connected,
                recovery_phase,
                unrecoverable_gap: None,
            }
        } else if recovery_phase != crate::models::recovery::RecoveryPhase::Live {
            ReadinessDiagnostics {
                state: PipelineReadinessState::Recovering,
                stage: Some(PipelineStage::Ingress),
                reason: Some(format!("recovery_{recovery_phase:?}").to_lowercase()),
                transport_connected,
                recovery_phase,
                unrecoverable_gap: None,
            }
        } else {
            ReadinessDiagnostics {
                state: pipeline_progress.readiness_state,
                stage: pipeline_progress.stale_stage,
                reason: pipeline_progress.readiness_reason.clone(),
                transport_connected,
                recovery_phase,
                unrecoverable_gap: None,
            }
        };

        Ok(HealthStatus {
            healthy: derive_health(
                redis_healthy,
                sqlite_available,
                session_count,
                &pipeline_progress,
                self.settings.pipeline_progress_readiness_enabled,
            ) && !containment.persistent,
            serving: dependency_healthy,
            recovering: readiness.state == PipelineReadinessState::Recovering,
            live: readiness.state == PipelineReadinessState::Healthy
                && recovery_phase == crate::models::recovery::RecoveryPhase::Live,
            stale: readiness.state == PipelineReadinessState::Stale,
            redis_connected: redis_healthy,
            sqlite_available,
            session_count,
            diagnostics,
            readiness,
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

        let pipeline_progress = self.progress.snapshot(self.progress_thresholds());
        self.collect_health_diagnostics(redis_connected, sqlite_available, pipeline_progress)
            .await
    }

    async fn collect_health_diagnostics(
        &self,
        redis_connected: bool,
        sqlite_available: bool,
        pipeline_progress: PipelineProgressSnapshot,
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
            negative_post_entries: cache.get_negative_post_entry_count(),
            negative_post_capacity: cache.get_negative_post_capacity(),
            negative_post_hits: cache_metrics.negative_post_hits,
            negative_post_evictions: cache_metrics.negative_post_evictions,
            post_recoveries: cache_metrics.post_recoveries,
            post_found: cache_metrics.post_found,
            post_missing: cache_metrics.post_missing,
            post_unavailable: cache_metrics.post_unavailable,
            partial_records_total: cache_metrics.partial_records,
            isolation_broad_outage: cache_metrics.isolation_broad_outage,
            isolation_singleton_poison: cache_metrics.isolation_singleton_poison,
            isolation_budget_exhausted: cache_metrics.isolation_budget_exhausted,
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
                partial_records: Some(snapshot.partial_records),
                vacuum_pending: Some(snapshot.vacuum_pending),
                vacuum_pending_reason: snapshot.vacuum_pending_reason,
                vacuum_pending_since: snapshot.vacuum_pending_since,
                vacuum_last_run_at: snapshot.vacuum_last_run_at,
                vacuum_last_run_duration_ms: snapshot.vacuum_last_run_duration_ms,
                vacuum_last_run_bytes_reclaimed: snapshot.vacuum_last_run_bytes_reclaimed,
                freelist_ratio: snapshot.freelist_ratio,
                over_budget: Some(snapshot.over_budget),
                over_budget_after_vacuum: Some(snapshot.over_budget_after_vacuum),
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
                partial_records: None,
                vacuum_pending: None,
                vacuum_pending_reason: None,
                vacuum_pending_since: None,
                vacuum_last_run_at: None,
                vacuum_last_run_duration_ms: None,
                vacuum_last_run_bytes_reclaimed: None,
                freelist_ratio: None,
                over_budget: None,
                over_budget_after_vacuum: None,
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
        let bluesky_fetch = self.bluesky_client.fetch_diagnostics().await;

        DiagnosticsCollector::assemble_health(
            process_memory,
            cache_state,
            sqlite_state,
            not_redis_state,
            pipeline_progress,
            self.failure_supervisor.snapshot(),
            bluesky_fetch,
        )
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
                    self.settings.vacuum_freelist_ratio,
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

        // Under budget: still evaluate proactive freelist-bloat reclamation
        // (no records are deleted in this path).
        self.sqlite_store
            .check_vacuum_bloat(max_size_bytes, self.settings.vacuum_freelist_ratio)
            .await?;
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

                // Awaited VACUUM scheduling: run a pending VACUUM inside the
                // low-traffic window, or immediately once it has been deferred
                // past the maximum defer duration.
                if let Err(e) = this.maybe_run_pending_vacuum().await {
                    error!("Scheduled VACUUM failed: {}", e);
                }
            }
        });
        info!(
            "Started database cleanup task (base: {}min, max: {}min, reset after {} skips)",
            base_interval_minutes, max_interval_minutes, reset_skip_count
        );
    }

    /// Runs a pending VACUUM when the current UTC hour is inside the configured
    /// low-traffic window, or when it has been pending longer than
    /// `vacuum_max_defer_hours`. Records the outcome in the store and updates
    /// the vacuum gauges.
    async fn maybe_run_pending_vacuum(&self) -> TurboResult<()> {
        let vacuum_state = self.sqlite_store.get_vacuum_state();

        if !vacuum_state.pending {
            return Ok(());
        }

        let now = Utc::now();
        let should_run = vacuum_should_run_now(
            now,
            vacuum_state.pending_since,
            self.settings.vacuum_window_start_hour,
            self.settings.vacuum_window_end_hour,
            self.settings.vacuum_max_defer_hours,
        );

        if !should_run {
            info!(
                reason = ?vacuum_state.pending_reason,
                pending_hours = vacuum_state
                    .pending_since
                    .map(|since| now.signed_duration_since(since).num_hours()),
                window = format_args!(
                    "{}:00-{}:00 UTC",
                    self.settings.vacuum_window_start_hour,
                    self.settings.vacuum_window_end_hour
                ),
                "VACUUM pending but outside low-traffic window; deferring"
            );
            self.emit_vacuum_gauges().await;
            return Ok(());
        }

        let max_size_bytes = (self.settings.max_db_size_mb as i64) * 1024 * 1024;
        let run = self.sqlite_store.run_vacuum(max_size_bytes).await?;
        info!(
            reason = ?vacuum_state.pending_reason,
            reclaimed_bytes = run.bytes_reclaimed,
            duration_ms = run.duration_ms,
            over_budget_after_vacuum = run.over_budget_after_vacuum,
            "Scheduled VACUUM completed"
        );
        self.emit_vacuum_gauges().await;
        Ok(())
    }

    /// Reflects the current SQLite vacuum state in Prometheus gauges.
    async fn emit_vacuum_gauges(&self) {
        match self.sqlite_store.get_state_snapshot().await {
            Ok(snapshot) => {
                metrics::gauge!("jetstream_turbo_db_size_bytes").set(snapshot.db_size_bytes as f64);
                if let Some(ratio) = snapshot.freelist_ratio {
                    metrics::gauge!("jetstream_turbo_db_freelist_ratio").set(ratio);
                }
                metrics::gauge!("jetstream_turbo_vacuum_pending").set(if snapshot.vacuum_pending {
                    1.0
                } else {
                    0.0
                });
                if let Some(duration_ms) = snapshot.vacuum_last_run_duration_ms {
                    metrics::gauge!("jetstream_turbo_vacuum_last_duration_ms")
                        .set(duration_ms as f64);
                }
                metrics::gauge!("jetstream_turbo_db_over_budget").set(if snapshot.over_budget {
                    1.0
                } else {
                    0.0
                });
            }
            Err(e) => warn!("Failed to read SQLite snapshot for vacuum gauges: {}", e),
        }
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
    use crate::client::ProfileFetcher;
    use crate::client::{
        BlueskyClient, BlueskyOperation, ContainmentPolicy, RequestRetryPolicy,
        UpstreamFailureCategory, UpstreamHttpError,
    };
    use crate::hydration::TurboCache;
    use crate::models::bluesky::BlueskyProfile;
    use crate::models::enriched::{EnrichedRecord, HydrationQuality};
    use crate::models::recovery::{SourceCursor, SourceEventId};
    use crate::storage::{EventPublisher, RecordStore};
    use crate::testing::{
        create_post_message, create_reply_message, MockEventPublisher, MockMessageSource,
        MockPostFetcher, MockProfileFetcher, MockRecordStore,
    };
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Notify;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn maintained_sqlite_store(
        path: &std::path::Path,
        pragma_config: SQLitePragmaConfig,
    ) -> SQLiteStore {
        SQLiteStore::maintain_schema(path, pragma_config, Duration::from_secs(1))
            .await
            .unwrap();
        SQLiteStore::new(path, pragma_config).await.unwrap()
    }

    struct CancellationFlag(Arc<AtomicBool>);
    impl Drop for CancellationFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct BlockingProfileFetcher {
        started: Arc<Notify>,
        cancelled: Arc<AtomicBool>,
    }
    impl ProfileFetcher for BlockingProfileFetcher {
        async fn bulk_fetch_profiles(
            &self,
            _dids: &[String],
        ) -> TurboResult<Vec<Option<BlueskyProfile>>> {
            let _cancel = CancellationFlag(Arc::clone(&self.cancelled));
            self.started.notify_one();
            std::future::pending().await
        }
    }

    struct BlockingRecordStore {
        started: Arc<Notify>,
        cancelled: Arc<AtomicBool>,
    }
    struct FailingRecordStore;
    impl RecordStore for FailingRecordStore {
        async fn store_batch(&self, _records: &[EnrichedRecord]) -> TurboResult<Vec<i64>> {
            Err(TurboError::Internal("sqlite write failed".to_string()))
        }
    }

    struct FailingEventPublisher;
    impl EventPublisher for FailingEventPublisher {
        async fn publish_batch(&self, _records: &[EnrichedRecord]) -> TurboResult<Vec<String>> {
            Err(TurboError::Internal("redis publication failed".to_string()))
        }
    }
    impl RecordStore for BlockingRecordStore {
        async fn store_batch(&self, _records: &[EnrichedRecord]) -> TurboResult<Vec<i64>> {
            let _cancel = CancellationFlag(Arc::clone(&self.cancelled));
            self.started.notify_one();
            std::future::pending().await
        }
    }

    struct BlockingEventPublisher {
        started: Arc<Notify>,
        cancelled: Arc<AtomicBool>,
    }

    struct DuplicateAwareRecordStore {
        completed: SourceEventId,
        store_called: Arc<AtomicBool>,
    }

    impl RecordStore for DuplicateAwareRecordStore {
        async fn store_batch(&self, _records: &[EnrichedRecord]) -> TurboResult<Vec<i64>> {
            self.store_called.store(true, Ordering::SeqCst);
            Ok(Vec::new())
        }

        async fn completed_source_event_ids(
            &self,
            _source_event_ids: &[SourceEventId],
        ) -> TurboResult<HashSet<SourceEventId>> {
            Ok(HashSet::from([self.completed.clone()]))
        }
    }
    impl EventPublisher for BlockingEventPublisher {
        async fn publish_batch(&self, _records: &[EnrichedRecord]) -> TurboResult<Vec<String>> {
            let _cancel = CancellationFlag(Arc::clone(&self.cancelled));
            self.started.notify_one();
            std::future::pending().await
        }
    }

    fn progress() -> Arc<PipelineProgress> {
        Arc::new(PipelineProgress::new(1, 10))
    }

    fn test_ingress_batch(messages: Vec<JetstreamMessage>) -> IngressBatch {
        let events = messages
            .into_iter()
            .enumerate()
            .map(|(index, message)| IngressEvent::new(index as u64 + 1, message).unwrap())
            .collect();
        ingress_batch(events).unwrap()
    }

    fn test_ingress_range(start: u64, end: u64) -> IngressRange {
        let cursor = |ordinal| SourceCursor {
            time_us: ordinal * 1_000,
            source_seq: Some(ordinal * 10),
            source_event_id: SourceEventId::from(format!("event-{ordinal}")),
        };
        IngressRange {
            start_ordinal: start,
            end_ordinal: end,
            start_cursor: cursor(start),
            end_cursor: cursor(end),
        }
    }

    fn replay_failure() -> TurboError {
        UpstreamHttpError {
            operation: BlueskyOperation::Profiles,
            status: Some(502),
            category: UpstreamFailureCategory::ServerError,
            diagnostic_summary: None,
            attempts: 2,
            retry_limit: 1,
            request_cardinality: 2,
            transient: true,
            request_fingerprint: "safe-replayed-batch".to_string(),
            isolation: None,
        }
        .into()
    }

    fn containment_policy() -> ContainmentPolicy {
        ContainmentPolicy {
            min_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(8),
            persistence_threshold: 2,
            isolation_request_budget: 4,
        }
    }

    #[test]
    fn cursorless_ingress_is_rejected_without_consuming_an_ordinal() {
        let progress = progress();
        let mut next_ordinal = 1;
        let mut cursorless = create_post_message(1);
        cursorless.time_us = None;

        let rejected = ProductionTurboCharger::accept_ingress_event(
            progress.as_ref(),
            &mut next_ordinal,
            cursorless,
        );
        let accepted = ProductionTurboCharger::accept_ingress_event(
            progress.as_ref(),
            &mut next_ordinal,
            create_post_message(2),
        )
        .expect("following valid event should be accepted");

        assert!(rejected.is_none());
        assert_eq!(accepted.ordinal, 1);
        assert_eq!(next_ordinal, 2);
        let snapshot = progress.snapshot(crate::turbocharger::ProgressThresholds {
            startup_grace: Duration::from_secs(1),
            ingress_idle: Duration::from_secs(1),
            batch_execution: Duration::from_secs(1),
            recovery_successes: 1,
        });
        assert_eq!(snapshot.rejected_ingress, 1);
        assert_eq!(snapshot.active_permits, 0);
    }

    #[tokio::test]
    async fn cursorless_rejection_never_checkpoints_and_following_completion_advances() {
        let progress = progress();
        let mut next_ordinal = 1;
        let mut cursorless = create_post_message(1);
        cursorless.time_us = None;
        assert!(ProductionTurboCharger::accept_ingress_event(
            progress.as_ref(),
            &mut next_ordinal,
            cursorless,
        )
        .is_none());

        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("cursorless-checkpoint.db");
        let sqlite = maintained_sqlite_store(
            &db_path,
            SQLitePragmaConfig {
                cache_size_kib: 1024,
                mmap_size_mb: 1,
                journal_size_limit_mb: 1,
            },
        )
        .await;
        assert_eq!(sqlite.load_ingestion_checkpoint().await.unwrap(), None);

        let accepted = ProductionTurboCharger::accept_ingress_event(
            progress.as_ref(),
            &mut next_ordinal,
            create_post_message(2),
        )
        .expect("valid event should follow rejection");
        let expected_time_us = accepted.cursor.time_us;
        let completion = TurboCharger::<
            MockMessageSource,
            MockProfileFetcher,
            MockPostFetcher,
            MockRecordStore,
            MockEventPublisher,
        >::process_batch_internal(
            Hydrator::new(
                TurboCache::new(10, 10),
                Arc::new(MockProfileFetcher::new()),
                Arc::new(MockPostFetcher::new()),
            ),
            Arc::new(MockRecordStore::new()),
            Arc::new(MockEventPublisher::new()),
            broadcast::channel(1).0,
            ingress_batch(vec![accepted]).unwrap(),
            Arc::clone(&progress),
            progress.batch_started(),
            Some(Duration::from_secs(1)),
        )
        .await
        .unwrap();
        let frontier = Mutex::new(CompletionFrontier::new(None));
        let checkpoint = persist_batch_completion(&frontier, &sqlite, completion)
            .await
            .unwrap()
            .expect("valid completion should checkpoint");

        assert_eq!(checkpoint.ingress_ordinal, 1);
        assert_eq!(checkpoint.cursor.time_us, expected_time_us);
    }

    #[test]
    fn repeated_cursorless_replay_remains_bounded_and_does_not_activate_work() {
        let progress = progress();
        let mut next_ordinal = 8;
        for _ in 0..16 {
            let mut cursorless = create_post_message(1);
            cursorless.time_us = None;
            assert!(ProductionTurboCharger::accept_ingress_event(
                progress.as_ref(),
                &mut next_ordinal,
                cursorless,
            )
            .is_none());
        }

        let snapshot = progress.snapshot(crate::turbocharger::ProgressThresholds {
            startup_grace: Duration::from_secs(1),
            ingress_idle: Duration::from_secs(1),
            batch_execution: Duration::from_secs(1),
            recovery_successes: 1,
        });
        assert_eq!(next_ordinal, 8);
        assert_eq!(snapshot.rejected_ingress, 16);
        assert_eq!(snapshot.rejected_ingress_reasons.len(), 1);
        assert_eq!(snapshot.rejected_ingress_kinds.len(), 1);
        assert_eq!(snapshot.active_permits, 0);
    }

    #[tokio::test]
    async fn unavailable_singleton_is_stored_published_and_checkpointed_as_partial() {
        let server = MockServer::start().await;
        let message = create_reply_message(1, "did:plc:parent", "poison");
        Mock::given(method("GET"))
            .and(path("/app.bsky.actor.getProfiles"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "profiles": [
                    {"did": message.did.clone(), "handle": "replier.bsky.social"},
                    {"did": "did:plc:parent", "handle": "parent.bsky.social"}
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.feed.getPosts"))
            .respond_with(ResponseTemplate::new(502))
            .expect(2)
            .mount(&server)
            .await;
        let bluesky = Arc::new(
            BlueskyClient::new_with_policies(
                vec!["test-session".to_string()],
                None,
                25,
                25,
                0,
                0,
                RequestRetryPolicy {
                    max_retries: 1,
                    base_delay: Duration::from_millis(1),
                    max_delay: Duration::from_millis(2),
                },
                containment_policy(),
            )
            .unwrap(),
        );
        bluesky.set_api_base_url_for_test(server.uri()).await;
        let hydrator = Hydrator::new(
            TurboCache::new_with_negative_cache(10, 10, 10, Duration::from_secs(300)),
            Arc::clone(&bluesky),
            Arc::clone(&bluesky),
        );
        let record_store = Arc::new(MockRecordStore::new());
        let publisher = Arc::new(MockEventPublisher::new());
        let progress = progress();

        let completion = TurboCharger::<
            MockMessageSource,
            BlueskyClient,
            BlueskyClient,
            MockRecordStore,
            MockEventPublisher,
        >::process_batch_internal(
            hydrator.clone(),
            Arc::clone(&record_store),
            Arc::clone(&publisher),
            broadcast::channel(1).0,
            test_ingress_batch(vec![message.clone()]),
            Arc::clone(&progress),
            progress.batch_started(),
            Some(Duration::from_secs(1)),
        )
        .await
        .unwrap();

        assert_eq!(record_store.get_stored_count().await, 1);
        assert_eq!(publisher.get_published_count().await, 1);
        assert_eq!(
            record_store.stored_records.lock().await[0]
                .hydrated_metadata
                .hydration_quality,
            HydrationQuality::Partial
        );

        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("optional-hydration-checkpoint.db");
        let sqlite = maintained_sqlite_store(
            &db_path,
            SQLitePragmaConfig {
                cache_size_kib: 1024,
                mmap_size_mb: 1,
                journal_size_limit_mb: 1,
            },
        )
        .await;
        let frontier = Mutex::new(CompletionFrontier::new(None));
        let checkpoint = persist_batch_completion(&frontier, &sqlite, completion)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            sqlite.load_ingestion_checkpoint().await.unwrap(),
            Some(checkpoint)
        );

        TurboCharger::<
            MockMessageSource,
            BlueskyClient,
            BlueskyClient,
            MockRecordStore,
            MockEventPublisher,
        >::process_batch_internal(
            hydrator,
            Arc::clone(&record_store),
            Arc::clone(&publisher),
            broadcast::channel(1).0,
            test_ingress_batch(vec![message]),
            Arc::clone(&progress),
            progress.batch_started(),
            Some(Duration::from_secs(1)),
        )
        .await
        .unwrap();
        assert_eq!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| { request.url.path().ends_with("getPosts") })
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn replayed_failure_increases_delay_and_keeps_checkpoint_blocked() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("replay-failure.db");
        let store = maintained_sqlite_store(
            &db_path,
            SQLitePragmaConfig {
                cache_size_kib: 1024,
                mmap_size_mb: 1,
                journal_size_limit_mb: 1,
            },
        )
        .await;
        let frontier = Mutex::new(CompletionFrontier::new(None));
        let supervisor = FailureSupervisor::new(containment_policy());

        let failed_range = test_ingress_range(9, 10);
        let first = supervisor.record_failure(&replay_failure(), Some(&failed_range), None);
        let replay = supervisor.record_failure(&replay_failure(), Some(&failed_range), None);
        persist_batch_completion(
            &frontier,
            &store,
            BatchCompletion {
                processed_count: 2,
                range: test_ingress_range(3, 4),
            },
        )
        .await
        .unwrap();

        assert!(replay.delay > first.delay);
        assert!(
            replay.persistent,
            "persistent containment drives stale readiness"
        );
        assert_eq!(supervisor.snapshot().recurrence, 2);
        assert_eq!(store.load_ingestion_checkpoint().await.unwrap(), None);
    }

    #[tokio::test]
    async fn replay_recovery_advances_checkpoint_and_resets_containment() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("replay-recovery.db");
        let store = maintained_sqlite_store(
            &db_path,
            SQLitePragmaConfig {
                cache_size_kib: 1024,
                mmap_size_mb: 1,
                journal_size_limit_mb: 1,
            },
        )
        .await;
        let frontier = Mutex::new(CompletionFrontier::new(None));
        let supervisor = FailureSupervisor::new(containment_policy());
        let failed_range = test_ingress_range(1, 2);
        supervisor.record_failure(&replay_failure(), Some(&failed_range), None);
        supervisor.record_failure(&replay_failure(), Some(&failed_range), None);

        let checkpoint = persist_batch_completion(
            &frontier,
            &store,
            BatchCompletion {
                processed_count: 2,
                range: test_ingress_range(1, 2),
            },
        )
        .await
        .unwrap()
        .expect("formerly blocked replay should advance checkpoint");
        assert!(supervisor.observe_checkpoint(&checkpoint).is_some());
        assert!(!supervisor.snapshot().active);

        let next = supervisor.record_failure(&replay_failure(), Some(&failed_range), None);
        assert_eq!(next.recurrence, 1);
        assert_eq!(next.delay, containment_policy().min_delay);
    }

    #[tokio::test]
    async fn checkpoint_persistence_failure_leaves_completion_replayable() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("checkpoint-retry.db");
        let pragma = SQLitePragmaConfig {
            cache_size_kib: 1024,
            mmap_size_mb: 1,
            journal_size_limit_mb: 1,
        };
        let failed_store = maintained_sqlite_store(&db_path, pragma).await;
        failed_store.close().await.unwrap();
        let frontier = Mutex::new(CompletionFrontier::new(None));

        let error = persist_batch_completion(
            &frontier,
            &failed_store,
            BatchCompletion {
                processed_count: 1,
                range: test_ingress_range(1, 1),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(error, TurboError::Database(_)));
        assert_eq!(frontier.lock().await.pending_range_count(), 0);

        let recovered_store = SQLiteStore::new(&db_path, pragma).await.unwrap();
        let checkpoint = persist_batch_completion(
            &frontier,
            &recovered_store,
            BatchCompletion {
                processed_count: 1,
                range: test_ingress_range(1, 1),
            },
        )
        .await
        .unwrap()
        .expect("same completion must be retryable after persistence recovers");
        assert_eq!(checkpoint.ingress_ordinal, 1);
    }

    #[test]
    fn resolve_batch_task_result_preserves_worker_error_and_failed_range() {
        let failed_range = test_ingress_range(7, 9);
        let result = ProductionTurboCharger::resolve_batch_task_result(Ok(Err(RunFailure::batch(
            TurboError::Internal("batch failed".to_string()),
            failed_range.clone(),
        ))));
        let failure = result.unwrap_err();
        assert!(matches!(
            failure.error(),
            TurboError::Internal(msg) if msg == "batch failed"
        ));
        assert_eq!(failure.failed_range(), Some(&failed_range));
    }

    #[tokio::test]
    async fn resolve_batch_task_result_propagates_join_error() {
        let join_error = tokio::spawn(async move {
            panic!("simulated worker panic");
            #[allow(unreachable_code)]
            Err::<BatchCompletion, TurboError>(TurboError::Internal("unreachable".to_string()))
        })
        .await
        .expect_err("task should panic");

        let result = ProductionTurboCharger::resolve_batch_task_result(Err(join_error));
        assert!(matches!(
            result,
            Err(failure) if matches!(failure.error(), TurboError::TaskJoin(_))
        ));
    }

    #[tokio::test]
    async fn later_batch_does_not_persist_past_unfinished_earlier_batch() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("checkpoint.db");
        let store = maintained_sqlite_store(
            &db_path,
            SQLitePragmaConfig {
                cache_size_kib: 1024,
                mmap_size_mb: 1,
                journal_size_limit_mb: 1,
            },
        )
        .await;
        let frontier = Mutex::new(CompletionFrontier::new(None));

        persist_batch_completion(
            &frontier,
            &store,
            BatchCompletion {
                processed_count: 2,
                range: test_ingress_range(3, 4),
            },
        )
        .await
        .unwrap();
        assert_eq!(store.load_ingestion_checkpoint().await.unwrap(), None);

        persist_batch_completion(
            &frontier,
            &store,
            BatchCompletion {
                processed_count: 2,
                range: test_ingress_range(1, 2),
            },
        )
        .await
        .unwrap();

        assert_eq!(
            store
                .load_ingestion_checkpoint()
                .await
                .unwrap()
                .unwrap()
                .ingress_ordinal,
            4
        );
    }

    #[test]
    fn batch_flush_counters_capture_mix_of_full_and_partial_batches() {
        let mut counters = BatchFlushCounters::default();

        counters.record(BATCH_SIZE, BatchFlushReason::Full, BATCH_SIZE);
        counters.record(BATCH_SIZE, BatchFlushReason::Timer, 12);
        counters.record(BATCH_SIZE, BatchFlushReason::Shutdown, 3);

        let snapshot = counters
            .snapshot(BATCH_SIZE)
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
        let mut counters = BatchFlushCounters::default();
        counters.record(BATCH_SIZE, BatchFlushReason::Timer, 7);

        assert!(counters.snapshot(BATCH_SIZE).is_some());
        counters.reset();
        assert!(counters.snapshot(BATCH_SIZE).is_none());
    }

    #[tokio::test]
    async fn hydration_timeout_is_typed_and_cancels_blocked_work() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let hydrator = Hydrator::new(
            TurboCache::new(10, 10),
            Arc::new(BlockingProfileFetcher {
                started: Arc::new(Notify::new()),
                cancelled: Arc::clone(&cancelled),
            }),
            Arc::new(MockPostFetcher::new()),
        );
        let progress = progress();
        let batch_id = progress.batch_started();
        let result = TurboCharger::<
            MockMessageSource,
            BlockingProfileFetcher,
            MockPostFetcher,
            MockRecordStore,
            MockEventPublisher,
        >::process_batch_internal(
            hydrator,
            Arc::new(MockRecordStore::new()),
            Arc::new(MockEventPublisher::new()),
            broadcast::channel(1).0,
            test_ingress_batch(vec![create_post_message(1)]),
            Arc::clone(&progress),
            batch_id,
            Some(Duration::from_millis(20)),
        )
        .await;
        assert!(
            matches!(result, Err(TurboError::BatchStageTimeout { stage, .. }) if stage == "hydration")
        );
        assert!(cancelled.load(Ordering::SeqCst));
        let snapshot = progress.snapshot(crate::turbocharger::ProgressThresholds {
            startup_grace: Duration::from_secs(1),
            ingress_idle: Duration::from_secs(1),
            batch_execution: Duration::from_secs(1),
            recovery_successes: 1,
        });
        assert_eq!(snapshot.timed_out_batches, 1);
        assert_eq!(snapshot.active_permits, 0);
    }

    #[tokio::test]
    async fn core_storage_and_publication_failures_produce_no_checkpointable_completion() {
        let storage_progress = progress();
        let storage_result = TurboCharger::<
            MockMessageSource,
            MockProfileFetcher,
            MockPostFetcher,
            FailingRecordStore,
            MockEventPublisher,
        >::process_batch_internal(
            Hydrator::new(
                TurboCache::new(10, 10),
                Arc::new(MockProfileFetcher::new()),
                Arc::new(MockPostFetcher::new()),
            ),
            Arc::new(FailingRecordStore),
            Arc::new(MockEventPublisher::new()),
            broadcast::channel(1).0,
            test_ingress_batch(vec![create_post_message(10)]),
            Arc::clone(&storage_progress),
            storage_progress.batch_started(),
            Some(Duration::from_secs(1)),
        )
        .await;
        assert!(
            matches!(storage_result, Err(TurboError::Internal(message)) if message == "sqlite write failed")
        );

        let publication_progress = progress();
        let publication_result = TurboCharger::<
            MockMessageSource,
            MockProfileFetcher,
            MockPostFetcher,
            MockRecordStore,
            FailingEventPublisher,
        >::process_batch_internal(
            Hydrator::new(
                TurboCache::new(10, 10),
                Arc::new(MockProfileFetcher::new()),
                Arc::new(MockPostFetcher::new()),
            ),
            Arc::new(MockRecordStore::new()),
            Arc::new(FailingEventPublisher),
            broadcast::channel(1).0,
            test_ingress_batch(vec![create_post_message(11)]),
            Arc::clone(&publication_progress),
            publication_progress.batch_started(),
            Some(Duration::from_secs(1)),
        )
        .await;
        assert!(
            matches!(publication_result, Err(TurboError::Internal(message)) if message == "redis publication failed")
        );
    }

    #[tokio::test]
    async fn storage_timeout_releases_permit_and_processing_can_resume() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let hydrator = Hydrator::new(
            TurboCache::new(10, 10),
            Arc::new(MockProfileFetcher::new()),
            Arc::new(MockPostFetcher::new()),
        );
        let progress = progress();
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&semaphore).acquire_owned().await.unwrap();
        let batch_id = progress.batch_started();
        let task = tokio::spawn({
            let progress = Arc::clone(&progress);
            async move {
                let _permit = permit;
                TurboCharger::<
                    MockMessageSource,
                    MockProfileFetcher,
                    MockPostFetcher,
                    BlockingRecordStore,
                    MockEventPublisher,
                >::process_batch_internal(
                    hydrator,
                    Arc::new(BlockingRecordStore {
                        started: Arc::new(Notify::new()),
                        cancelled: Arc::clone(&cancelled),
                    }),
                    Arc::new(MockEventPublisher::new()),
                    broadcast::channel(1).0,
                    test_ingress_batch(vec![create_post_message(2)]),
                    progress,
                    batch_id,
                    Some(Duration::from_millis(20)),
                )
                .await
            }
        });
        let result = task.await.unwrap();
        assert!(
            matches!(result, Err(TurboError::BatchStageTimeout { stage, .. }) if stage == "storage")
        );
        assert_eq!(semaphore.available_permits(), 1);

        let resumed_id = progress.batch_started();
        let resumed = TurboCharger::<
            MockMessageSource,
            MockProfileFetcher,
            MockPostFetcher,
            MockRecordStore,
            MockEventPublisher,
        >::process_batch_internal(
            Hydrator::new(
                TurboCache::new(10, 10),
                Arc::new(MockProfileFetcher::new()),
                Arc::new(MockPostFetcher::new()),
            ),
            Arc::new(MockRecordStore::new()),
            Arc::new(MockEventPublisher::new()),
            broadcast::channel(1).0,
            test_ingress_batch(vec![create_post_message(3)]),
            Arc::clone(&progress),
            resumed_id,
            Some(Duration::from_secs(1)),
        )
        .await;
        let completion = resumed.unwrap();
        assert_eq!(completion.processed_count, 1);
        assert_eq!(completion.range.end_ordinal, 1);
    }

    #[tokio::test]
    async fn publication_timeout_reports_publication_stage() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let progress = progress();
        let batch_id = progress.batch_started();
        let result = TurboCharger::<
            MockMessageSource,
            MockProfileFetcher,
            MockPostFetcher,
            MockRecordStore,
            BlockingEventPublisher,
        >::process_batch_internal(
            Hydrator::new(
                TurboCache::new(10, 10),
                Arc::new(MockProfileFetcher::new()),
                Arc::new(MockPostFetcher::new()),
            ),
            Arc::new(MockRecordStore::new()),
            Arc::new(BlockingEventPublisher {
                started: Arc::new(Notify::new()),
                cancelled: Arc::clone(&cancelled),
            }),
            broadcast::channel(1).0,
            test_ingress_batch(vec![create_post_message(4)]),
            Arc::clone(&progress),
            batch_id,
            Some(Duration::from_millis(20)),
        )
        .await;
        assert!(
            matches!(result, Err(TurboError::BatchStageTimeout { stage, .. }) if stage == "publication")
        );
        assert!(cancelled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn completed_replay_duplicate_skips_hydration_and_storage() {
        let mut message = create_post_message(5);
        message.time_us = Some(1);
        let source_event_id = SourceEventId::from_message(&message);
        let store_called = Arc::new(AtomicBool::new(false));
        let publisher = Arc::new(MockEventPublisher::new());
        let (broadcast_sender, mut broadcast_receiver) = broadcast::channel(1);
        let progress = progress();
        let batch_id = progress.batch_started();
        let result = TurboCharger::<
            MockMessageSource,
            BlockingProfileFetcher,
            MockPostFetcher,
            DuplicateAwareRecordStore,
            MockEventPublisher,
        >::process_batch_internal(
            Hydrator::new(
                TurboCache::new(10, 10),
                Arc::new(BlockingProfileFetcher {
                    started: Arc::new(Notify::new()),
                    cancelled: Arc::new(AtomicBool::new(false)),
                }),
                Arc::new(MockPostFetcher::new()),
            ),
            Arc::new(DuplicateAwareRecordStore {
                completed: source_event_id,
                store_called: Arc::clone(&store_called),
            }),
            Arc::clone(&publisher),
            broadcast_sender,
            test_ingress_batch(vec![message]),
            progress,
            batch_id,
            Some(Duration::from_millis(20)),
        )
        .await
        .unwrap();

        assert_eq!(result.processed_count, 0);
        assert!(!store_called.load(Ordering::SeqCst));
        assert_eq!(publisher.call_count.load(Ordering::SeqCst), 0);
        assert!(broadcast_receiver.try_recv().is_err());
        assert_eq!(result.range.end_ordinal, 1);
    }

    #[test]
    fn vacuum_window_is_half_open_and_wraps_past_midnight() {
        // Default 03:00-05:00 window.
        assert!(in_vacuum_window(3, 3, 5));
        assert!(in_vacuum_window(4, 3, 5));
        assert!(!in_vacuum_window(5, 3, 5), "end hour is exclusive");
        assert!(!in_vacuum_window(2, 3, 5));
        assert!(!in_vacuum_window(23, 3, 5));

        // Overnight window 22:00-02:00.
        assert!(in_vacuum_window(22, 22, 2));
        assert!(in_vacuum_window(23, 22, 2));
        assert!(in_vacuum_window(0, 22, 2));
        assert!(in_vacuum_window(1, 22, 2));
        assert!(!in_vacuum_window(2, 22, 2));
        assert!(!in_vacuum_window(12, 22, 2));
    }

    #[test]
    fn pending_vacuum_stays_pending_outside_window_before_defer_elapses() {
        let now = Utc::now();
        let pending_since = now - chrono::Duration::hours(1);

        // Outside the window (assuming the current hour is not 3-4) and well
        // under the 6h defer: must NOT run.
        let outside_hour = (now.hour() + 6) % 24;
        assert!(!in_vacuum_window(outside_hour, now.hour(), now.hour() + 1));
        assert!(!vacuum_should_run_now(
            now,
            Some(pending_since),
            outside_hour,
            (outside_hour + 1) % 24,
            6,
        ));
    }

    #[test]
    fn pending_vacuum_runs_inside_window() {
        let now = Utc::now();
        let current_hour = now.hour();
        let window_start = current_hour;
        let window_end = (current_hour + 1) % 24;

        // Inside the window, even freshly pending: must run.
        assert!(vacuum_should_run_now(
            now,
            Some(now),
            window_start,
            window_end,
            6,
        ));
        // Without a pending-since timestamp the window alone decides.
        assert!(vacuum_should_run_now(
            now,
            None,
            window_start,
            window_end,
            6
        ));
    }

    #[test]
    fn pending_vacuum_runs_after_max_defer_hours_even_outside_window() {
        let now = Utc::now();
        let pending_since = now - chrono::Duration::hours(7);
        let outside_hour = (now.hour() + 6) % 24;

        // Outside the window but past the 6h defer: must run regardless.
        assert!(!in_vacuum_window(outside_hour, now.hour(), now.hour() + 1));
        assert!(vacuum_should_run_now(
            now,
            Some(pending_since),
            outside_hour,
            (outside_hour + 1) % 24,
            6,
        ));

        // Just under the defer limit: must stay pending.
        let recent = now - chrono::Duration::hours(5);
        assert!(!vacuum_should_run_now(
            now,
            Some(recent),
            outside_hour,
            (outside_hour + 1) % 24,
            6,
        ));
    }
}
