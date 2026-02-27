use crate::stream::{ConnectionStatus, StreamId, StreamMessage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStats {
    pub stream_a: u64,
    pub stream_b: u64,
    pub delta: i64,
    pub rate_a: f64,
    pub rate_b: f64,
    pub stream_a_name: String,
    pub stream_b_name: String,
    pub timestamp: DateTime<Utc>,
    pub uptime_a: f64,
    pub uptime_b: f64,
    pub connected_a: bool,
    pub connected_b: bool,
}

pub struct StatsAggregator {
    tx: broadcast::Sender<StreamStats>,
    stream_a_name: String,
    stream_b_name: String,
}

impl StatsAggregator {
    pub fn new(stream_a_name: String, stream_b_name: String) -> Self {
        let (tx, _) = broadcast::channel(16);
        Self {
            tx,
            stream_a_name,
            stream_b_name,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StreamStats> {
        self.tx.subscribe()
    }

    pub fn sender(&self) -> broadcast::Sender<StreamStats> {
        self.tx.clone()
    }

    pub fn process(&self, stats: &Arc<std::sync::RwLock<StreamStatsInternal>>, uptime: &Arc<std::sync::RwLock<UptimeTracker>>) {
        let tx = self.tx.clone();
        let stats = Arc::clone(stats);
        let uptime = Arc::clone(uptime);
        let stream_a_name = self.stream_a_name.clone();
        let stream_b_name = self.stream_b_name.clone();

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

                    let (uptime_a, uptime_b, connected_a, connected_b) = {
                        let up = uptime.read().unwrap();
                        let (a, b) = up.get_current_uptime_seconds();
                        (a, b, up.connected_a, up.connected_b)
                    };

                    let stats_snapshot = StreamStats {
                        stream_a: internal.count_a,
                        stream_b: internal.count_b,
                        delta: internal.count_a as i64 - internal.count_b as i64,
                        rate_a: rate_ema_a,
                        rate_b: rate_ema_b,
                        stream_a_name: stream_a_name.clone(),
                        stream_b_name: stream_b_name.clone(),
                        timestamp: Utc::now(),
                        uptime_a: uptime_a as f64,
                        uptime_b: uptime_b as f64,
                        connected_a,
                        connected_b,
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

#[derive(Debug, Default)]
pub struct UptimeTracker {
    pub uptime_a_seconds: u64,
    pub uptime_b_seconds: u64,
    pub connected_a: bool,
    pub connected_b: bool,
    pub connected_at_a: Option<Instant>,
    pub connected_at_b: Option<Instant>,
}

impl UptimeTracker {
    pub fn handle_connection_status(&mut self, status: ConnectionStatus) {
        match status.stream_id {
            StreamId::A => {
                if status.connected {
                    self.connected_a = true;
                    self.connected_at_a = Some(Instant::now());
                } else {
                    if let Some(connected_at) = self.connected_at_a.take() {
                        self.uptime_a_seconds += connected_at.elapsed().as_secs();
                    }
                    self.connected_a = false;
                }
            }
            StreamId::B => {
                if status.connected {
                    self.connected_b = true;
                    self.connected_at_b = Some(Instant::now());
                } else {
                    if let Some(connected_at) = self.connected_at_b.take() {
                        self.uptime_b_seconds += connected_at.elapsed().as_secs();
                    }
                    self.connected_b = false;
                }
            }
        }
    }

    pub fn get_current_uptime_seconds(&self) -> (u64, u64) {
        let mut a = self.uptime_a_seconds;
        let mut b = self.uptime_b_seconds;
        
        if let Some(connected_at) = self.connected_at_a {
            a += connected_at.elapsed().as_secs();
        }
        if let Some(connected_at) = self.connected_at_b {
            b += connected_at.elapsed().as_secs();
        }
        
        (a, b)
    }
}
