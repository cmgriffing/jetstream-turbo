use crate::models::{
    enriched::EnrichedRecord,
    errors::{TurboError, TurboResult},
};
use not_redis::Client as NotRedisClient;
use serde_json;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info, trace};

pub trait EventPublisher {
    fn publish_batch(
        &self,
        records: &[EnrichedRecord],
    ) -> impl std::future::Future<Output = TurboResult<Vec<String>>> + Send;
}

pub struct RedisStore {
    client: Arc<Mutex<NotRedisClient>>,
    stream_name: String,
    max_length: Option<usize>,
}

impl RedisStore {
    pub async fn new(
        _redis_url: &str,
        stream_name: String,
        max_length: Option<usize>,
    ) -> TurboResult<Self> {
        info!("Connecting to not_redis with stream: {}", stream_name);

        let client = NotRedisClient::new();
        client.start().await;

        info!("Connected to not_redis, using stream: {}", stream_name);

        Ok(Self {
            client: Arc::new(Mutex::new(client)),
            stream_name,
            max_length,
        })
    }

    pub async fn publish_record(&self, record: &EnrichedRecord) -> TurboResult<String> {
        let message_id = generate_message_id(record);
        let values = publication_values(record)?;

        let mut client = self.client.lock().await;
        let id: String = client
            .xadd(self.stream_name.clone(), Some(&message_id), values)
            .await
            .map_err(TurboError::RedisOperation)?;

        if let Some(max_len) = self.max_length {
            let _: i64 = client
                .xtrim(self.stream_name.clone(), max_len, false)
                .await
                .map_err(TurboError::RedisOperation)?;
        }

        trace!("Published record to not_redis stream with ID: {}", id);
        Ok(id)
    }

    pub async fn get_stream_info(&self) -> TurboResult<StreamInfo> {
        let mut client = self.client.lock().await;
        let stream_length: i64 = client
            .xlen(self.stream_name.clone())
            .await
            .map_err(TurboError::RedisOperation)?;

        let redis_version = "not_redis".to_string();

        Ok(StreamInfo {
            redis_version,
            stream_length: stream_length as usize,
            stream_name: self.stream_name.clone(),
            max_length: self.max_length,
        })
    }

    pub async fn clear_stream(&self) -> TurboResult<()> {
        info!("Clearing not_redis stream: {}", self.stream_name);
        let mut client = self.client.lock().await;

        let _: i64 = client
            .del(self.stream_name.clone())
            .await
            .map_err(TurboError::RedisOperation)?;

        trace!("Cleared not_redis stream: {}", self.stream_name);
        Ok(())
    }

    pub async fn health_check(&self) -> TurboResult<bool> {
        let mut client = self.client.lock().await;
        match client.ping().await {
            Ok(_) => Ok(true),
            Err(e) => {
                error!("not_redis health check failed: {}", e);
                Ok(false)
            }
        }
    }

    pub fn get_stream_name(&self) -> &str {
        &self.stream_name
    }

    pub fn get_max_length(&self) -> Option<usize> {
        self.max_length
    }
}

impl EventPublisher for RedisStore {
    async fn publish_batch(&self, records: &[EnrichedRecord]) -> TurboResult<Vec<String>> {
        if records.is_empty() {
            return Ok(vec![]);
        }

        let mut client = self.client.lock().await;
        let mut message_ids = Vec::with_capacity(records.len());

        // Batch Redis operations - acquire lock once for all records
        for record in records {
            let message_id = generate_message_id(record);
            let values = publication_values(record)?;

            let id: String = client
                .xadd(self.stream_name.clone(), Some(&message_id), values)
                .await
                .map_err(TurboError::RedisOperation)?;

            message_ids.push(id);
        }

        // Trim stream once after batch if needed
        if let Some(max_len) = self.max_length {
            let _: i64 = client
                .xtrim(self.stream_name.clone(), max_len, false)
                .await
                .map_err(TurboError::RedisOperation)?;
        }

        info!(
            "Published batch of {} records to not_redis stream",
            records.len()
        );
        Ok(message_ids)
    }
}

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub redis_version: String,
    pub stream_length: usize,
    pub stream_name: String,
    pub max_length: Option<usize>,
}

fn generate_message_id(record: &EnrichedRecord) -> String {
    format!(
        "{}-{}",
        record.processed_at.timestamp_millis(),
        record.message.seq.unwrap_or(0)
    )
}

fn publication_values(record: &EnrichedRecord) -> TurboResult<Vec<(&'static str, String)>> {
    Ok(vec![
        ("at_uri", record.get_at_uri().unwrap_or_default()),
        ("did", record.get_did().to_string()),
        ("source_event_id", record.source_event_id().to_string()),
        ("message", serde_json::to_string(record)?),
        ("hydrated_at", record.processed_at.to_rfc3339()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::enriched::ProcessingMetrics;

    #[test]
    fn test_generate_message_id() {
        let record = EnrichedRecord {
            message: crate::models::jetstream::JetstreamMessage {
                did: "did:plc:test".to_string(),
                seq: Some(12345),
                time_us: Some(1640995200000000),
                kind: crate::models::jetstream::MessageKind::Commit,
                commit: Some(crate::models::jetstream::CommitData {
                    rev: Some("test-rev".to_string()),
                    operation_type: crate::models::jetstream::OperationType::Create,
                    collection: Some("app.bsky.feed.post".to_string()),
                    rkey: Some("test".to_string()),
                    record: Some(serde_json::json!({"text": "Hello world"})),
                    cid: Some("bafyrei".to_string()),
                }),
            },
            hydrated_metadata: crate::models::enriched::HydratedMetadata::default(),
            processed_at: chrono::Utc::now(),
            metrics: ProcessingMetrics {
                hydration_time_ms: 100,
                api_calls_count: 2,
                cache_hit_rate: 0.8,
                cache_hits: 8,
                cache_misses: 2,
            },
        };

        let message_id = generate_message_id(&record);
        assert!(message_id.contains('-'));
        assert_eq!(message_id.split('-').count(), 2);
    }

    #[test]
    fn replay_publication_keeps_source_identity_across_crash_boundary() {
        let original = EnrichedRecord::new_with_timestamp(
            crate::testing::create_post_message(42),
            chrono::DateTime::UNIX_EPOCH,
        );
        let replayed = EnrichedRecord::new_with_timestamp(
            original.message.clone(),
            chrono::DateTime::UNIX_EPOCH + chrono::Duration::seconds(1),
        );
        let source_id = |record: &EnrichedRecord| {
            publication_values(record)
                .unwrap()
                .into_iter()
                .find_map(|(key, value)| (key == "source_event_id").then_some(value))
                .unwrap()
        };

        assert_ne!(
            generate_message_id(&original),
            generate_message_id(&replayed)
        );
        assert_eq!(source_id(&original), source_id(&replayed));
    }
}
