use std::time::Duration;
use futures::StreamExt;
use tracing::{debug, info};
use crate::models::{
    jetstream::JetstreamMessage,
    enriched::EnrichedRecord,
    errors::TurboError,
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
    
    pub async fn process_stream<S>(&self, mut stream: S) -> TurboResult<impl futures::Stream<Item = TurboResult<EnrichedRecord>>>
    where
        S: futures::Stream<Item = TurboResult<JetstreamMessage>> + Unpin,
    {
        use futures::stream;
        
        let mut buffer = Vec::with_capacity(self.batch_size);
        let mut last_flush = tokio::time::Instant::now();
        
        let processed_stream = stream::unfold(buffer, move |mut buffer| async move {
            // Collect items for the next batch
            while buffer.len() < self.batch_size && last_flush.elapsed() < self.max_wait_time {
                match stream.next().await {
                    Some(Ok(message)) => buffer.push(message),
                    Some(Err(e)) => return Some((Err(e), buffer)),
                    None => break,
                }
            }
            
            if buffer.is_empty() {
                return None;
            }
            
            // Process the batch
            let current_batch = std::mem::take(&mut buffer);
            last_flush = tokio::time::Instant::now();
            
            match self.hydrator.hydrate_batch(current_batch).await {
                Ok(processed) => {
                    debug!("Processed batch of {} records", processed.len());
                    Some((Ok(futures::stream::iter(processed)), buffer))
                }
                Err(e) => Some((Err(e), buffer)),
            }
        })
        .flatten();
        
        Ok(processed_stream)
    }
    
    pub fn get_batch_size(&self) -> usize {
        self.batch_size
    }
    
    pub fn set_batch_size(&mut self, batch_size: usize) {
        self.batch_size = batch_size;
    }
}