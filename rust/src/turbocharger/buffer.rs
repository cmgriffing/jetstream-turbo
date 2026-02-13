use std::collections::VecDeque;
use std::time::{Duration, Instant};
use crate::models::jetstream::JetstreamMessage;

pub struct MessageBuffer {
    messages: VecDeque<JetstreamMessage>,
    batch_size: usize,
    max_wait_time: Duration,
    last_flush: Instant,
}

impl MessageBuffer {
    pub fn new(batch_size: usize, max_wait_time: Duration) -> Self {
        Self {
            messages: VecDeque::new(),
            batch_size,
            max_wait_time,
            last_flush: Instant::now(),
        }
    }
    
    pub fn add(&mut self, message: JetstreamMessage) -> bool {
        self.messages.push_back(message);
        self.should_flush()
    }
    
    pub fn is_ready(&self) -> bool {
        self.should_flush()
    }
    
    pub fn drain(&mut self) -> Vec<JetstreamMessage> {
        self.last_flush = Instant::now();
        self.messages.drain(..).collect()
    }
    
    pub fn len(&self) -> usize {
        self.messages.len()
    }
    
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
    
    fn should_flush(&self) -> bool {
        self.messages.len() >= self.batch_size || 
        (!self.messages.is_empty() && self.last_flush.elapsed() >= self.max_wait_time)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::jetstream::{CommitData, Operation, Record};
    use chrono::Utc;
    
    fn create_test_message(seq: u64) -> JetstreamMessage {
        JetstreamMessage {
            did: "did:plc:test".to_string(),
            seq,
            time_us: 1640995200000000,
            commit: CommitData {
                seq,
                rebase: false,
                time_us: 1640995200000000,
                operation: Operation::Create {
                    record: Record {
                        uri: format!("at://did:plc:test/app.bsky.feed.post/{}", seq),
                        cid: "bafyrei".to_string(),
                        author: "did:plc:test".to_string(),
                        r#type: "app.bsky.feed.post".to_string(),
                        created_at: Utc::now(),
                        fields: serde_json::json!({"text": format!("Test message {}", seq)}),
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
        }
    }
    
    #[test]
    fn test_message_buffer_basic() {
        let mut buffer = MessageBuffer::new(3, Duration::from_secs(5));
        
        assert!(buffer.is_empty());
        assert!(!buffer.is_ready());
        assert_eq!(buffer.len(), 0);
        
        // Add first message
        let msg1 = create_test_message(1);
        let ready = buffer.add(msg1);
        
        assert!(!ready);
        assert!(!buffer.is_ready());
        assert_eq!(buffer.len(), 1);
        
        // Add second message
        let msg2 = create_test_message(2);
        let ready = buffer.add(msg2);
        
        assert!(!ready);
        assert!(!buffer.is_ready());
        assert_eq!(buffer.len(), 2);
        
        // Add third message - should trigger flush
        let msg3 = create_test_message(3);
        let ready = buffer.add(msg3);
        
        assert!(ready);
        assert!(buffer.is_ready());
        assert_eq!(buffer.len(), 3);
    }
    
    #[test]
    fn test_message_buffer_drain() {
        let mut buffer = MessageBuffer::new(2, Duration::from_secs(5));
        
        buffer.add(create_test_message(1));
        buffer.add(create_test_message(2));
        
        assert_eq!(buffer.len(), 2);
        assert!(buffer.is_ready());
        
        let messages = buffer.drain();
        
        assert_eq!(messages.len(), 2);
        assert!(buffer.is_empty());
        assert!(!buffer.is_ready());
        assert_eq!(buffer.len(), 0);
    }
    
    #[tokio::test]
    async fn test_message_buffer_time_based_flush() {
        let mut buffer = MessageBuffer::new(10, Duration::from_millis(100));
        
        buffer.add(create_test_message(1));
        
        assert!(!buffer.is_ready());
        
        // Wait for time-based flush
        tokio::time::sleep(Duration::from_millis(150)).await;
        
        assert!(buffer.is_ready());
    }
}