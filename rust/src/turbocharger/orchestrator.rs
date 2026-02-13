use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use futures::StreamExt;
use tracing::{error, info};
use serde::Serialize;
use crate::config::Settings;
use crate::client::{JetstreamClient, BlueskyClient, BlueskyAuthClient};
use crate::hydration::{Hydrator, TurboCache};
use crate::storage::{SQLiteStore, RedisStore};
use crate::models::{
    jetstream::JetstreamMessage,
    errors::{TurboError, TurboResult},
};

pub struct TurboCharger {
    settings: Settings,
    jetstream_client: JetstreamClient,
    bluesky_client: Arc<BlueskyClient>,
    auth_client: BlueskyAuthClient,
    hydrator: Hydrator,
    sqlite_store: SQLiteStore,
    redis_store: RedisStore,
    semaphore: Arc<Semaphore>,
}

impl TurboCharger {
    pub async fn new(settings: Settings, modulo: u32, shard: u32) -> TurboResult<Self> {
        info!("Initializing TurboCharger with modulo={}, shard={}", modulo, shard);
        
        // Initialize Jetstream client
        let jetstream_client = JetstreamClient::with_defaults(settings.jetstream_hosts.clone());
        
        // Authenticate directly with Bluesky
        let auth_client = BlueskyAuthClient::new(
            settings.bluesky_handle.clone(),
            settings.bluesky_app_password.clone(),
        );
        
        let session_string = auth_client.authenticate().await?;
        info!("Successfully authenticated with Bluesky as {}", settings.bluesky_handle);
        
        let bluesky_client = Arc::new(BlueskyClient::new(vec![session_string]));
        
        // Initialize cache
        let cache = TurboCache::new(settings.cache_size_users, settings.cache_size_posts);
        
        // Initialize hydrator
        let hydrator = Hydrator::new(cache, bluesky_client.clone());
        
        // Initialize storage
        let sqlite_store = SQLiteStore::new(&settings.db_dir).await?;
        
        let redis_store = RedisStore::new(
            &settings.redis_url,
            settings.stream_name_redis.clone(),
            settings.trim_maxlen,
        ).await?;
        
        // Initialize semaphore for concurrency control
        let semaphore = Arc::new(Semaphore::new(settings.max_concurrent_requests));
        
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
        })
    }
    
    pub async fn run(&self) -> TurboResult<()> {
        info!("Starting TurboCharger main loop");
        
        let message_stream = self.jetstream_client.stream_messages().await?;
        
        let mut processed_count = 0u64;
        let mut last_stats = std::time::Instant::now();
        
        tokio::pin!(message_stream);
        
        while let Some(result) = message_stream.next().await {
            match result {
                Ok(message) => {
                    // Apply sharding filter if specified
                    if self.should_process_message(&message) {
                        let permit = self.semaphore.acquire().await.unwrap();
                        
                        match self.process_message(message).await {
                            Ok(_) => {
                                processed_count += 1;
                            }
                            Err(e) => {
                                error!("Failed to process message: {}", e);
                            }
                        }
                        
                        drop(permit);
                    }
                }
                Err(e) => {
                    error!("Error receiving message from Jetstream: {}", e);
                    // Continue processing other messages
                }
            }
            
            // Print stats every 30 seconds
            if last_stats.elapsed() >= Duration::from_secs(30) {
                let (user_hit_rate, post_hit_rate) = self.hydrator.get_cache().get_hit_rates().await;
                info!(
                    "Processed {} messages. Cache hit rates: users={:.2}%, posts={:.2}%",
                    processed_count,
                    user_hit_rate * 100.0,
                    post_hit_rate * 100.0
                );
                
                last_stats = std::time::Instant::now();
            }
        }
        
        error!("Jetstream stream ended unexpectedly");
        Err(TurboError::Internal("Jetstream stream ended".to_string()))
    }
    
    async fn process_message(&self, message: JetstreamMessage) -> TurboResult<()> {
        // Only process create operations (skip updates and deletes for now)
        if !message.is_create_operation() {
            return Ok(());
        }
        
        // Hydrate the message
        let enriched = self.hydrator.hydrate_message(message).await?;
        
        // Store in SQLite
        let _record_id = self.sqlite_store.store_record(&enriched).await?;
        
        // Publish to Redis stream
        let _message_id = self.redis_store.publish_record(&enriched).await?;
        
        Ok(())
    }
    
    fn should_process_message(&self, _message: &JetstreamMessage) -> bool {
        // Apply modulo-based sharding if specified
        // For now, just return true
        true
    }
    
    pub async fn refresh_sessions(&self) -> TurboResult<()> {
        info!("Refreshing Bluesky session");
        
        let session_string = self.auth_client.authenticate().await?;
        self.bluesky_client.refresh_sessions(vec![session_string]).await;
        
        info!("Refreshed session for {}", self.settings.bluesky_handle);
        Ok(())
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
        let sqlite_count = self.sqlite_store.count_records().await.ok();
        
        Ok(HealthStatus {
            healthy: redis_healthy,
            redis_connected: redis_healthy,
            sqlite_available: sqlite_count.is_some(),
            session_count: self.bluesky_client.get_session_count().await,
        })
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