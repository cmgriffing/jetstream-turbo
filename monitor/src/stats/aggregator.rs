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
    pub latency_a_ms: u64,
    pub latency_b_ms: u64,
    pub current_streak_a: f64,
    pub current_streak_b: f64,
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

    pub fn process(
        &self,
        stats: &Arc<std::sync::RwLock<StreamStatsInternal>>,
        uptime: &Arc<std::sync::RwLock<UptimeTracker>>,
    ) {
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

                    let (
                        uptime_a,
                        uptime_b,
                        connected_a,
                        connected_b,
                        latency_a,
                        latency_b,
                        streak_a,
                        streak_b,
                    ) = {
                        let up = uptime.read().unwrap();
                        let (a, b) = up.get_current_uptime_seconds();
                        (
                            a,
                            b,
                            up.connected_a,
                            up.connected_b,
                            up.get_avg_latency_a(),
                            up.get_avg_latency_b(),
                            up.get_current_streak_a(),
                            up.get_current_streak_b(),
                        )
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
                        latency_a_ms: latency_a,
                        latency_b_ms: latency_b,
                        current_streak_a: streak_a,
                        current_streak_b: streak_b,
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

#[derive(Debug)]
pub struct UptimeTracker {
    pub connected_a: bool,
    pub connected_b: bool,
    pub connected_at_a: Option<Instant>,
    pub connected_at_b: Option<Instant>,
    pub disconnect_count_a: u64,
    pub disconnect_count_b: u64,
    pub latency_sum_a_ms: u64,
    pub latency_sum_b_ms: u64,
    pub latency_count_a: u64,
    pub latency_count_b: u64,
    pub total_messages_a: u64,
    pub total_messages_b: u64,
    session_start_a: Option<Instant>,
    session_start_b: Option<Instant>,
    connected_seconds_a: u64,
    connected_seconds_b: u64,
    disconnected_at_a: Option<Instant>,
    disconnected_at_b: Option<Instant>,
}

impl Default for UptimeTracker {
    fn default() -> Self {
        Self {
            connected_a: false,
            connected_b: false,
            connected_at_a: None,
            connected_at_b: None,
            disconnect_count_a: 0,
            disconnect_count_b: 0,
            latency_sum_a_ms: 0,
            latency_sum_b_ms: 0,
            latency_count_a: 0,
            latency_count_b: 0,
            total_messages_a: 0,
            total_messages_b: 0,
            session_start_a: None,
            session_start_b: None,
            connected_seconds_a: 0,
            connected_seconds_b: 0,
            disconnected_at_a: None,
            disconnected_at_b: None,
        }
    }
}

impl UptimeTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle_connection_status(&mut self, status: ConnectionStatus) {
        let now = Instant::now();
        
        match status.stream_id {
            StreamId::A => {
                if status.connected {
                    self.session_start_a = Some(now);
                    self.connected_a = true;
                    self.connected_at_a = Some(now);
                    self.disconnected_at_a = None;
                    if let Some(latency) = status.latency_ms {
                        self.latency_sum_a_ms += latency;
                        self.latency_count_a += 1;
                    }
                } else {
                    if let Some(session_start) = self.session_start_a.take() {
                        let elapsed = now.duration_since(session_start).as_secs();
                        self.connected_seconds_a = self.connected_seconds_a.saturating_add(elapsed);
                    }
                    self.connected_a = false;
                    self.disconnected_at_a = Some(now);
                    self.disconnect_count_a += 1;
                }
            }
            StreamId::B => {
                if status.connected {
                    self.session_start_b = Some(now);
                    self.connected_b = true;
                    self.connected_at_b = Some(now);
                    self.disconnected_at_b = None;
                    if let Some(latency) = status.latency_ms {
                        self.latency_sum_b_ms += latency;
                        self.latency_count_b += 1;
                    }
                } else {
                    if let Some(session_start) = self.session_start_b.take() {
                        let elapsed = now.duration_since(session_start).as_secs();
                        self.connected_seconds_b = self.connected_seconds_b.saturating_add(elapsed);
                    }
                    self.connected_b = false;
                    self.disconnected_at_b = Some(now);
                    self.disconnect_count_b += 1;
                }
            }
        }
    }

    pub fn record_message(&mut self, stream_id: StreamId) {
        match stream_id {
            StreamId::A => self.total_messages_a += 1,
            StreamId::B => self.total_messages_b += 1,
        }
    }

    pub fn get_current_uptime_seconds(&self) -> (u64, u64) {
        let now = Instant::now();
        
        let uptime_a = if self.connected_a {
            10000
        } else {
            let connected_time = self.connected_seconds_a;
            let disconnected_time = self.disconnected_at_a
                .map(|d| now.duration_since(d).as_secs())
                .unwrap_or(0);
            
            if connected_time == 0 && disconnected_time == 0 {
                0
            } else {
                let total = connected_time + disconnected_time;
                ((connected_time as f64 / total as f64) * 10000.0) as u64
            }
        };
        
        let uptime_b = if self.connected_b {
            10000
        } else {
            let connected_time = self.connected_seconds_b;
            let disconnected_time = self.disconnected_at_b
                .map(|d| now.duration_since(d).as_secs())
                .unwrap_or(0);
            
            if connected_time == 0 && disconnected_time == 0 {
                0
            } else {
                let total = connected_time + disconnected_time;
                ((connected_time as f64 / total as f64) * 10000.0) as u64
            }
        };

        (uptime_a, uptime_b)
    }

    pub fn get_avg_latency_a(&self) -> u64 {
        if self.latency_count_a > 0 {
            self.latency_sum_a_ms / self.latency_count_a
        } else {
            0
        }
    }

    pub fn get_avg_latency_b(&self) -> u64 {
        if self.latency_count_b > 0 {
            self.latency_sum_b_ms / self.latency_count_b
        } else {
            0
        }
    }

    pub fn get_current_streak_a(&self) -> f64 {
        if let Some(connected_at) = self.connected_at_a {
            connected_at.elapsed().as_secs() as f64
        } else {
            0.0
        }
    }

    pub fn get_current_streak_b(&self) -> f64 {
        if let Some(connected_at) = self.connected_at_b {
            connected_at.elapsed().as_secs() as f64
        } else {
            0.0
        }
    }

    pub fn get_detailed_stats(&self, period_seconds: u64) -> UptimeDetailedStats {
        let (current_a, current_b) = self.get_current_uptime_seconds();
        let avg_latency_a = self.get_avg_latency_a();
        let avg_latency_b = self.get_avg_latency_b();

        let rate_a = if current_a > 0 {
            self.total_messages_a as f64 / current_a as f64
        } else {
            0.0
        };
        let rate_b = if current_b > 0 {
            self.total_messages_b as f64 / current_b as f64
        } else {
            0.0
        };

        UptimeDetailedStats {
            uptime_a_seconds: current_a,
            uptime_b_seconds: current_b,
            uptime_a_percent: if period_seconds > 0 {
                (current_a as f64 / period_seconds as f64) * 100.0
            } else {
                0.0
            },
            uptime_b_percent: if period_seconds > 0 {
                (current_b as f64 / period_seconds as f64) * 100.0
            } else {
                0.0
            },
            disconnect_count_a: self.disconnect_count_a,
            disconnect_count_b: self.disconnect_count_b,
            avg_latency_a_ms: avg_latency_a,
            avg_latency_b_ms: avg_latency_b,
            total_messages_a: self.total_messages_a,
            total_messages_b: self.total_messages_b,
            avg_rate_a: rate_a,
            avg_rate_b: rate_b,
            connected_a: self.connected_a,
            connected_b: self.connected_b,
            current_streak_a: self.get_current_streak_a(),
            current_streak_b: self.get_current_streak_b(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UptimeDetailedStats {
    pub uptime_a_seconds: u64,
    pub uptime_b_seconds: u64,
    pub uptime_a_percent: f64,
    pub uptime_b_percent: f64,
    pub disconnect_count_a: u64,
    pub disconnect_count_b: u64,
    pub avg_latency_a_ms: u64,
    pub avg_latency_b_ms: u64,
    pub total_messages_a: u64,
    pub total_messages_b: u64,
    pub avg_rate_a: f64,
    pub avg_rate_b: f64,
    pub connected_a: bool,
    pub connected_b: bool,
    pub current_streak_a: f64,
    pub current_streak_b: f64,
}
