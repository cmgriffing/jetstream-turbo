use redis::{AsyncCommands, Client as RedisClient, aio::MultiplexedConnection};
use serde_json;
use tracing::{debug, error, info};
use crate::models::{
    enriched::EnrichedRecord,
    errors::{TurboError, TurboResult},
};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct RedisStore {
    client: RedisClient,
    connection: Arc<Mutex<MultiplexedConnection>>,
    stream_name: String,
    max_length: Option<usize>,
}

impl RedisStore {
    pub async fn new(redis_url: &str, stream_name: String, max_length: Option<usize>) -> TurboResult<Self> {
        info!("Connecting to Redis at: {}", redis_url);

        let client = RedisClient::open(redis_url)?;
        let connection = client.get_multiplexed_async_connection().await?;

        info!("Connected to Redis, using stream: {}", stream_name);

        Ok(Self {
            client,
            connection: Arc::new(Mutex::new(connection)),
            stream_name,
            max_length,
        })
    }

    pub async fn publish_record(&self, record: &EnrichedRecord) -> TurboResult<String> {
        let message_json = serde_json::to_string(record)?;
        let message_id = generate_message_id(&record);
        let mut conn = self.connection.lock().await;

        // Add to Redis stream
        let _: () = conn
            .xadd(
                &self.stream_name,
                &message_id,
                &[
                    ("at_uri", record.get_at_uri().unwrap_or("")),
                    ("did", record.get_did()),
                    ("message", &message_json),
                    ("hydrated_at", &record.processed_at.to_rfc3339()),
                ]
            )
            .await
            .map_err(|e| TurboError::RedisOperation(e))?;

        // Trim stream if max_length is set
        if let Some(max_len) = self.max_length {
            let _: () = redis::cmd("XTRIM")
                .arg(&self.stream_name)
                .arg("MAXLEN")
                .arg(max_len)
                .query_async(&mut *conn)
                .await
                .map_err(|e| TurboError::RedisOperation(e))?;
        }

        debug!("Published record to Redis stream with ID: {}", message_id);
        Ok(message_id)
    }

    pub async fn publish_batch(&self, records: &[EnrichedRecord]) -> TurboResult<Vec<String>> {
        let mut message_ids = Vec::with_capacity(records.len());

        for record in records {
            let message_id = self.publish_record(record).await?;
            message_ids.push(message_id);
        }

        info!("Published batch of {} records to Redis stream", records.len());
        Ok(message_ids)
    }

    pub async fn get_stream_info(&self) -> TurboResult<StreamInfo> {
        let mut conn = self.connection.lock().await;
        
        // Use redis::cmd for info command
        let info: String = redis::cmd("INFO")
            .query_async(&mut *conn)
            .await
            .map_err(|e| TurboError::RedisOperation(e))?;

        let stream_length: usize = conn
            .xlen(&self.stream_name)
            .await
            .map_err(|e| TurboError::RedisOperation(e))?;

        // Parse redis version from info string
        let redis_version = info
            .lines()
            .find(|line| line.starts_with("redis_version:"))
            .and_then(|line| line.split(':').nth(1))
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        Ok(StreamInfo {
            redis_version,
            stream_length,
            stream_name: self.stream_name.clone(),
            max_length: self.max_length,
        })
    }

    pub async fn clear_stream(&self) -> TurboResult<()> {
        info!("Clearing Redis stream: {}", self.stream_name);
        let mut conn = self.connection.lock().await;

        let _: () = redis::cmd("DEL")
            .arg(&self.stream_name)
            .query_async(&mut *conn)
            .await
            .map_err(|e| TurboError::RedisOperation(e))?;

        debug!("Cleared Redis stream: {}", self.stream_name);
        Ok(())
    }

    pub async fn health_check(&self) -> TurboResult<bool> {
        let mut conn = self.connection.lock().await;
        // Use redis::cmd for ping command
        let result: Result<String, redis::RedisError> = redis::cmd("PING")
            .query_async(&mut *conn)
            .await;
        match result {
            Ok(_) => Ok(true),
            Err(e) => {
                error!("Redis health check failed: {}", e);
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
    // Generate a message ID based on the record's timestamp and sequence
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
