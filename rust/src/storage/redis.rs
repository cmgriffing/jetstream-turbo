use not_redis::Client as NotRedisClient;
use serde_json;
use tokio::sync::Mutex;
use std::sync::Arc;
use tracing::{debug, error, info};
use crate::models::{
    enriched::EnrichedRecord,
    errors::{TurboError, TurboResult},
};

pub struct RedisStore {
    client: Arc<Mutex<NotRedisClient>>,
    stream_name: String,
    max_length: Option<usize>,
}

impl RedisStore {
    pub async fn new(_redis_url: &str, stream_name: String, max_length: Option<usize>) -> TurboResult<Self> {
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
        let message_json = serde_json::to_string(record)?;
        let message_id = generate_message_id(record);
        let at_uri = record.get_at_uri().unwrap_or("");
        let did = record.get_did();
        let hydrated_at = record.processed_at.to_rfc3339();

        let values = vec![
            ("at_uri", at_uri),
            ("did", did),
            ("message", &message_json),
            ("hydrated_at", &hydrated_at),
        ];

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

        debug!("Published record to not_redis stream with ID: {}", id);
        Ok(id)
    }

    pub async fn publish_batch(&self, records: &[EnrichedRecord]) -> TurboResult<Vec<String>> {
        let mut message_ids = Vec::with_capacity(records.len());

        for record in records {
            let message_id = self.publish_record(record).await?;
            message_ids.push(message_id);
        }

        info!("Published batch of {} records to not_redis stream", records.len());
        Ok(message_ids)
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

        debug!("Cleared not_redis stream: {}", self.stream_name);
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

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub redis_version: String,
    pub stream_length: usize,
    pub stream_name: String,
    pub max_length: Option<usize>,
}

fn generate_message_id(record: &EnrichedRecord) -> String {
    format!("{}-{}", 
        record.processed_at.timestamp_millis(),
        record.message.seq
    )
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
                seq: 12345,
                time_us: 1640995200000000,
                commit: crate::models::jetstream::CommitData {
                    seq: 12345,
                    rebase: false,
                    time_us: 1640995200000000,
                    operation: crate::models::jetstream::Operation::Create {
                        record: crate::models::jetstream::Record {
                            uri: "at://did:plc:test/app.bsky.feed.post/test".to_string(),
                            cid: "bafyrei".to_string(),
                            author: "did:plc:test".to_string(),
                            r#type: "app.bsky.feed.post".to_string(),
                            created_at: chrono::Utc::now(),
                            fields: serde_json::json!({"text": "Hello world"}),
                            embed: None,
                            labels: None,
                            langs: None,
                            reply: None,
                            tags: None,
                            facets: None,
                            collections: None,
                        }
                    }
                }
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
}
