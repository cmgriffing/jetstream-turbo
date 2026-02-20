use crate::stream::{StreamId, StreamMessage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStats {
    pub stream_a: u64,
    pub stream_b: u64,
    pub delta: i64,
    pub rate_a: f64,
    pub rate_b: f64,
    pub timestamp: DateTime<Utc>,
}

pub struct StatsAggregator {
    tx: broadcast::Sender<StreamStats>,
}

impl StatsAggregator {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(16);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StreamStats> {
        self.tx.subscribe()
    }

    pub fn sender(&self) -> broadcast::Sender<StreamStats> {
        self.tx.clone()
    }

    pub fn process(&self, stats: &Arc<std::sync::RwLock<StreamStatsInternal>>) {
        let tx = self.tx.clone();
        let stats = Arc::clone(stats);

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
            let mut last_a: u64 = 0;
            let mut last_b: u64 = 0;
            let mut last_time = std::time::Instant::now();
            let mut rate_ema_a: f64 = 0.0;
            let mut rate_ema_b: f64 = 0.0;
            const ALPHA: f64 = 0.3;

            loop {
                interval.tick().await;

                let internal = stats.read().unwrap();
                let now = std::time::Instant::now();
                let elapsed = now.duration_since(last_time).as_secs_f64();

                if elapsed > 0.0 {
                    let instant_rate_a = (internal.count_a.saturating_sub(last_a)) as f64 / elapsed;
                    let instant_rate_b = (internal.count_b.saturating_sub(last_b)) as f64 / elapsed;

                    rate_ema_a = ALPHA * instant_rate_a + (1.0 - ALPHA) * rate_ema_a;
                    rate_ema_b = ALPHA * instant_rate_b + (1.0 - ALPHA) * rate_ema_b;

                    let stats_snapshot = StreamStats {
                        stream_a: internal.count_a,
                        stream_b: internal.count_b,
                        delta: internal.count_b as i64 - internal.count_a as i64,
                        rate_a: rate_ema_a,
                        rate_b: rate_ema_b,
                        timestamp: Utc::now(),
                    };

                    last_a = internal.count_a;
                    last_b = internal.count_b;
                    last_time = now;

                    let _ = tx.send(stats_snapshot);
                }
            }
        });
    }
}

#[derive(Debug, Default)]
pub struct StreamStatsInternal {
    pub count_a: u64,
    pub count_b: u64,
}

impl StreamStatsInternal {
    pub fn update(&mut self, msg: StreamMessage) {
        match msg.stream_id {
            StreamId::A => self.count_a = msg.count,
            StreamId::B => self.count_b = msg.count,
        }
    }
}
