use std::time::Duration;
use futures::StreamExt;
use tracing::{debug, info};
use crate::models::{
    jetstream::JetstreamMessage,
    enriched::EnrichedRecord,
    TurboResult,
};
use crate::hydration::Hydrator;

pub struct BatchProcessor {
    hydrator: Hydrator,
    batch_size: usize,
    max_wait_time: Duration,
}

impl BatchProcessor {
    pub fn new(hydrator: Hydrator, batch_size: usize, max_wait_time: Duration) -> Self {
        Self {
            hydrator,
            batch_size,
            max_wait_time,
        }
    }
    
    pub async fn process_stream<S>(&self, mut stream: S) -> TurboResult<Vec<EnrichedRecord>>
    where
        S: futures::Stream<Item = TurboResult<JetstreamMessage>> + Unpin,
    {
        let mut buffer = Vec::with_capacity(self.batch_size);
        let mut results = Vec::new();
        let mut last_flush = tokio::time::Instant::now();
        
        while let Some(item) = stream.next().await {
            match item {
                Ok(message) => {
                    buffer.push(message);
                    
                    // Flush if batch is full or max wait time reached
                    if buffer.len() >= self.batch_size || last_flush.elapsed() >= self.max_wait_time {
                        let batch = std::mem::take(&mut buffer);
                        match self.hydrator.hydrate_batch(batch).await {
                            Ok(processed) => {
                                debug!("Processed batch of {} records", processed.len());
                                results.extend(processed);
                            }
                            Err(e) => return Err(e),
                        }
                        last_flush = tokio::time::Instant::now();
                    }
                }
                Err(e) => return Err(e),
            }
        }
        
        // Process remaining items in buffer
        if !buffer.is_empty() {
            match self.hydrator.hydrate_batch(buffer).await {
                Ok(processed) => {
                    debug!("Processed final batch of {} records", processed.len());
                    results.extend(processed);
                }
                Err(e) => return Err(e),
            }
        }
        
        info!("Total processed {} records", results.len());
        Ok(results)
    }
    
    pub fn get_batch_size(&self) -> usize {
        self.batch_size
    }
    
    pub fn set_batch_size(&mut self, batch_size: usize) {
        self.batch_size = batch_size;
    }
}
