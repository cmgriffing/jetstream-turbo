use crate::stream::{ConnectionStatus, StreamId, StreamMessage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStats {
    pub stream_a: u64,
    pub stream_b: u64,
    pub counting_started_at: DateTime<Utc>,
    pub delta: i64,
    pub rate_a: f64,
    pub rate_b: f64,
    pub stream_a_name: String,
    pub stream_b_name: String,
    pub timestamp: DateTime<Utc>,
    pub uptime_a: f64,
    pub uptime_b: f64,
    pub uptime_a_all_time: f64,
    pub uptime_b_all_time: f64,
    pub downtime_a: f64,
    pub downtime_b: f64,
    pub connected_a: bool,
    pub connected_b: bool,
    pub connect_time_a_ms: u64,
    pub connect_time_b_ms: u64,
    pub delivery_latency_a_ms: f64,
    pub delivery_latency_b_ms: f64,
    pub mttr_a_ms: u64,
    pub mttr_b_ms: u64,
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
        let counting_started_at = Utc::now();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));

            loop {
                interval.tick().await;

                let internal = stats.read().unwrap();

                let (
                    rate_a,
                    rate_b,
                    connected_a,
                    connected_b,
                    connect_time_a,
                    connect_time_b,
                    delivery_latency_a,
                    delivery_latency_b,
                    mttr_a,
                    mttr_b,
                ) = {
                    let up = uptime.read().unwrap();
                    let (rate_a, rate_b) = up.get_average_rates();
                    (
                        rate_a,
                        rate_b,
                        up.connected_a,
                        up.connected_b,
                        up.get_avg_connect_time_a(),
                        up.get_avg_connect_time_b(),
                        up.get_delivery_latency_a_ms(),
                        up.get_delivery_latency_b_ms(),
                        up.get_mttr_a_ms(),
                        up.get_mttr_b_ms(),
                    )
                };

                let (
                    uptime_a,
                    uptime_b,
                    streak_a,
                    streak_b,
                    (uptime_a_all_time, uptime_b_all_time),
                    (downtime_a, downtime_b),
                ) = {
                    let up = uptime.read().unwrap();
                    let (a, b) = up.get_current_uptime_percentage();
                    (
                        a,
                        b,
                        up.get_current_streak_a(),
                        up.get_current_streak_b(),
                        up.get_all_time_uptime_from_downtime(),
                        up.get_all_time_downtime_seconds(),
                    )
                };

                let stats_snapshot = StreamStats {
                    stream_a: internal.total_a,
                    stream_b: internal.total_b,
                    counting_started_at: counting_started_at.clone(),
                    delta: internal.total_a as i64 - internal.total_b as i64,
                    rate_a,
                    rate_b,
                    stream_a_name: stream_a_name.clone(),
                    stream_b_name: stream_b_name.clone(),
                    timestamp: Utc::now(),
                    uptime_a,
                    uptime_b,
                    uptime_a_all_time,
                    uptime_b_all_time,
                    downtime_a: downtime_a as f64,
                    downtime_b: downtime_b as f64,
                    connected_a,
                    connected_b,
                    connect_time_a_ms: connect_time_a,
                    connect_time_b_ms: connect_time_b,
                    delivery_latency_a_ms: delivery_latency_a,
                    delivery_latency_b_ms: delivery_latency_b,
                    mttr_a_ms: mttr_a,
                    mttr_b_ms: mttr_b,
                    current_streak_a: streak_a,
                    current_streak_b: streak_b,
                };

                let _ = tx.send(stats_snapshot);
            }
        });
    }
}

#[derive(Debug, Default)]
pub struct StreamStatsInternal {
    pub total_a: u64,
    pub total_b: u64,
}

impl StreamStatsInternal {
    pub fn update(&mut self, msg: StreamMessage) {
        match msg.stream_id {
            StreamId::A => self.total_a = msg.count,
            StreamId::B => self.total_b = msg.count,
        }
    }

    pub fn load_totals(&mut self, total_a: u64, total_b: u64) {
        self.total_a = total_a;
        self.total_b = total_b;
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
    pub connect_time_sum_a_ms: u64,
    pub connect_time_sum_b_ms: u64,
    pub connect_time_count_a: u64,
    pub connect_time_count_b: u64,
    pub total_messages_a: u64,
    pub total_messages_b: u64,
    message_samples_a: VecDeque<(Instant, u64)>,
    message_samples_b: VecDeque<(Instant, u64)>,
    session_start_a: Option<Instant>,
    session_start_b: Option<Instant>,
    connected_seconds_a: u64,
    connected_seconds_b: u64,
    disconnected_seconds_a: u64,
    disconnected_seconds_b: u64,
    disconnected_at_a: Option<Instant>,
    disconnected_at_b: Option<Instant>,
    session_start_disconnected_a: Option<Instant>,
    session_start_disconnected_b: Option<Instant>,
    server_start_time: Instant,
    delivery_latency_samples_a: VecDeque<(Instant, u64)>,
    delivery_latency_samples_b: VecDeque<(Instant, u64)>,
    total_recovery_time_a_ms: u64,
    total_recovery_time_b_ms: u64,
    recovery_count_a: u64,
    recovery_count_b: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct UptimeMetricsSnapshot {
    pub uptime_a_seconds: u64,
    pub uptime_b_seconds: u64,
    pub downtime_a_seconds: u64,
    pub downtime_b_seconds: u64,
    pub disconnect_count_a: u64,
    pub disconnect_count_b: u64,
    pub connect_time_sum_a_ms: u64,
    pub connect_time_sum_b_ms: u64,
    pub connect_time_count_a: u64,
    pub connect_time_count_b: u64,
    pub total_messages_a: u64,
    pub total_messages_b: u64,
    pub total_recovery_time_a_ms: u64,
    pub total_recovery_time_b_ms: u64,
    pub recovery_count_a: u64,
    pub recovery_count_b: u64,
    pub delivery_latency_a_ms: f64,
    pub delivery_latency_b_ms: f64,
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
            connect_time_sum_a_ms: 0,
            connect_time_sum_b_ms: 0,
            connect_time_count_a: 0,
            connect_time_count_b: 0,
            total_messages_a: 0,
            total_messages_b: 0,
            message_samples_a: VecDeque::new(),
            message_samples_b: VecDeque::new(),
            session_start_a: None,
            session_start_b: None,
            connected_seconds_a: 0,
            connected_seconds_b: 0,
            disconnected_seconds_a: 0,
            disconnected_seconds_b: 0,
            disconnected_at_a: None,
            disconnected_at_b: None,
            session_start_disconnected_a: None,
            session_start_disconnected_b: None,
            server_start_time: Instant::now(),
            delivery_latency_samples_a: VecDeque::new(),
            delivery_latency_samples_b: VecDeque::new(),
            total_recovery_time_a_ms: 0,
            total_recovery_time_b_ms: 0,
            recovery_count_a: 0,
            recovery_count_b: 0,
        }
    }
}

impl UptimeTracker {
    const RATE_WINDOW: Duration = Duration::from_secs(10);

    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle_connection_status(&mut self, status: ConnectionStatus) {
        let now = Instant::now();

        match status.stream_id {
            StreamId::A => {
                if status.connected {
                    if !self.connected_a {
                        if let Some(session_start) = self.session_start_disconnected_a.take() {
                            let elapsed = now.duration_since(session_start).as_secs();
                            self.disconnected_seconds_a =
                                self.disconnected_seconds_a.saturating_add(elapsed);
                        }
                        if let Some(disc_at) = self.disconnected_at_a {
                            let recovery_ms = now.duration_since(disc_at).as_millis() as u64;
                            self.total_recovery_time_a_ms += recovery_ms;
                            self.recovery_count_a += 1;
                        }
                    }
                    self.session_start_a = Some(now);
                    self.connected_a = true;
                    self.connected_at_a = Some(now);
                    self.disconnected_at_a = None;
                    if let Some(ct) = status.connect_time_ms {
                        self.connect_time_sum_a_ms += ct;
                        self.connect_time_count_a += 1;
                    }
                } else {
                    if let Some(session_start) = self.session_start_a.take() {
                        let elapsed = now.duration_since(session_start).as_secs();
                        self.connected_seconds_a = self.connected_seconds_a.saturating_add(elapsed);
                    }
                    self.connected_a = false;
                    self.disconnected_at_a = Some(now);
                    self.session_start_disconnected_a = Some(now);
                    self.disconnect_count_a += 1;
                }
            }
            StreamId::B => {
                if status.connected {
                    if !self.connected_b {
                        if let Some(session_start) = self.session_start_disconnected_b.take() {
                            let elapsed = now.duration_since(session_start).as_secs();
                            self.disconnected_seconds_b =
                                self.disconnected_seconds_b.saturating_add(elapsed);
                        }
                        if let Some(disc_at) = self.disconnected_at_b {
                            let recovery_ms = now.duration_since(disc_at).as_millis() as u64;
                            self.total_recovery_time_b_ms += recovery_ms;
                            self.recovery_count_b += 1;
                        }
                    }
                    self.session_start_b = Some(now);
                    self.connected_b = true;
                    self.connected_at_b = Some(now);
                    self.disconnected_at_b = None;
                    if let Some(ct) = status.connect_time_ms {
                        self.connect_time_sum_b_ms += ct;
                        self.connect_time_count_b += 1;
                    }
                } else {
                    if let Some(session_start) = self.session_start_b.take() {
                        let elapsed = now.duration_since(session_start).as_secs();
                        self.connected_seconds_b = self.connected_seconds_b.saturating_add(elapsed);
                    }
                    self.connected_b = false;
                    self.disconnected_at_b = Some(now);
                    self.session_start_disconnected_b = Some(now);
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

    pub fn record_total_count(&mut self, stream_id: StreamId, total_count: u64) {
        let now = Instant::now();

        match stream_id {
            StreamId::A => {
                self.total_messages_a = total_count;
                Self::record_sample(&mut self.message_samples_a, now, total_count);
            }
            StreamId::B => {
                self.total_messages_b = total_count;
                Self::record_sample(&mut self.message_samples_b, now, total_count);
            }
        }
    }

    pub fn record_delivery_latency(&mut self, stream_id: StreamId, latency_us: u64) {
        let now = Instant::now();
        match stream_id {
            StreamId::A => {
                self.delivery_latency_samples_a.push_back((now, latency_us));
                while let Some((t, _)) = self.delivery_latency_samples_a.front() {
                    if now.duration_since(*t) > Self::RATE_WINDOW {
                        self.delivery_latency_samples_a.pop_front();
                    } else {
                        break;
                    }
                }
            }
            StreamId::B => {
                self.delivery_latency_samples_b.push_back((now, latency_us));
                while let Some((t, _)) = self.delivery_latency_samples_b.front() {
                    if now.duration_since(*t) > Self::RATE_WINDOW {
                        self.delivery_latency_samples_b.pop_front();
                    } else {
                        break;
                    }
                }
            }
        }
    }

    fn avg_delivery_latency_ms(samples: &VecDeque<(Instant, u64)>, now: Instant) -> f64 {
        let valid: Vec<_> = samples
            .iter()
            .filter(|(t, _)| now.duration_since(*t) <= Self::RATE_WINDOW)
            .collect();
        if valid.is_empty() {
            return 0.0;
        }
        let sum: u64 = valid.iter().map(|(_, v)| v).sum();
        (sum as f64 / valid.len() as f64) / 1000.0 // us -> ms
    }

    pub fn get_delivery_latency_a_ms(&self) -> f64 {
        Self::avg_delivery_latency_ms(&self.delivery_latency_samples_a, Instant::now())
    }

    pub fn get_delivery_latency_b_ms(&self) -> f64 {
        Self::avg_delivery_latency_ms(&self.delivery_latency_samples_b, Instant::now())
    }

    pub fn get_mttr_a_ms(&self) -> u64 {
        if self.recovery_count_a > 0 {
            self.total_recovery_time_a_ms / self.recovery_count_a
        } else {
            0
        }
    }

    pub fn get_mttr_b_ms(&self) -> u64 {
        if self.recovery_count_b > 0 {
            self.total_recovery_time_b_ms / self.recovery_count_b
        } else {
            0
        }
    }

    fn record_sample(samples: &mut VecDeque<(Instant, u64)>, now: Instant, total_count: u64) {
        samples.push_back((now, total_count));

        while let Some((sample_time, _)) = samples.front() {
            if now.duration_since(*sample_time) > Self::RATE_WINDOW {
                samples.pop_front();
            } else {
                break;
            }
        }
    }

    fn rolling_rate(samples: &VecDeque<(Instant, u64)>, now: Instant) -> f64 {
        let (latest_time, latest_total) = match samples.back() {
            Some(sample) => sample,
            None => return 0.0,
        };

        if now.duration_since(*latest_time) > Self::RATE_WINDOW {
            return 0.0;
        }

        let (oldest_time, oldest_total) = match samples.front() {
            Some(sample) => sample,
            None => return 0.0,
        };

        let elapsed = latest_time.duration_since(*oldest_time).as_secs_f64();
        if elapsed <= 0.0 {
            return 0.0;
        }

        latest_total.saturating_sub(*oldest_total) as f64 / elapsed
    }

    pub fn get_all_time_uptime_seconds(&self) -> (u64, u64) {
        let now = Instant::now();

        let uptime_a = if self.connected_a {
            let current_session = self
                .connected_at_a
                .map(|start| now.duration_since(start).as_secs())
                .unwrap_or(0);
            self.connected_seconds_a.saturating_add(current_session)
        } else {
            self.connected_seconds_a
        };

        let uptime_b = if self.connected_b {
            let current_session = self
                .connected_at_b
                .map(|start| now.duration_since(start).as_secs())
                .unwrap_or(0);
            self.connected_seconds_b.saturating_add(current_session)
        } else {
            self.connected_seconds_b
        };

        (uptime_a, uptime_b)
    }

    pub fn get_current_uptime_percentage(&self) -> (f64, f64) {
        let now = Instant::now();

        let uptime_a = if self.connected_a {
            100.0
        } else {
            let connected_time = self.connected_seconds_a;
            let disconnected_time = self
                .disconnected_at_a
                .map(|d| now.duration_since(d).as_secs())
                .unwrap_or(0);

            if connected_time == 0 && disconnected_time == 0 {
                0.0
            } else {
                let total = connected_time + disconnected_time;
                (connected_time as f64 / total as f64) * 100.0
            }
        };

        let uptime_b = if self.connected_b {
            100.0
        } else {
            let connected_time = self.connected_seconds_b;
            let disconnected_time = self
                .disconnected_at_b
                .map(|d| now.duration_since(d).as_secs())
                .unwrap_or(0);

            if connected_time == 0 && disconnected_time == 0 {
                0.0
            } else {
                let total = connected_time + disconnected_time;
                (connected_time as f64 / total as f64) * 100.0
            }
        };

        (uptime_a, uptime_b)
    }

    pub fn get_all_time_uptime_percentage(&self) -> (f64, f64) {
        let (uptime_a, uptime_b) = self.get_all_time_uptime_seconds();
        let (downtime_a, downtime_b) = self.get_all_time_downtime_seconds();

        let total_a = uptime_a.saturating_add(downtime_a);
        let total_b = uptime_b.saturating_add(downtime_b);

        let uptime_pct_a = if total_a > 0 {
            (uptime_a as f64 / total_a as f64) * 100.0
        } else {
            0.0
        };
        let uptime_pct_b = if total_b > 0 {
            (uptime_b as f64 / total_b as f64) * 100.0
        } else {
            0.0
        };

        (uptime_pct_a.min(100.0), uptime_pct_b.min(100.0))
    }

    pub fn get_all_time_downtime_seconds(&self) -> (u64, u64) {
        let now = Instant::now();

        let downtime_a = if self.connected_a {
            self.disconnected_seconds_a
        } else {
            let current_disconnect = self
                .disconnected_at_a
                .map(|d| now.duration_since(d).as_secs())
                .unwrap_or(0);
            self.disconnected_seconds_a
                .saturating_add(current_disconnect)
        };

        let downtime_b = if self.connected_b {
            self.disconnected_seconds_b
        } else {
            let current_disconnect = self
                .disconnected_at_b
                .map(|d| now.duration_since(d).as_secs())
                .unwrap_or(0);
            self.disconnected_seconds_b
                .saturating_add(current_disconnect)
        };

        (downtime_a, downtime_b)
    }

    pub fn get_all_time_uptime_from_downtime(&self) -> (f64, f64) {
        let now = Instant::now();
        let server_run_time = now.duration_since(self.server_start_time).as_secs();

        if server_run_time == 0 {
            return (0.0, 0.0);
        }

        let (downtime_a, downtime_b) = self.get_all_time_downtime_seconds();

        let uptime_a = 100.0 - ((downtime_a as f64 / server_run_time as f64) * 100.0);
        let uptime_b = 100.0 - ((downtime_b as f64 / server_run_time as f64) * 100.0);

        (uptime_a.max(0.0).min(100.0), uptime_b.max(0.0).min(100.0))
    }

    pub fn get_metrics_snapshot(&self) -> UptimeMetricsSnapshot {
        let (uptime_a_seconds, uptime_b_seconds) = self.get_all_time_uptime_seconds();
        let (downtime_a_seconds, downtime_b_seconds) = self.get_all_time_downtime_seconds();

        UptimeMetricsSnapshot {
            uptime_a_seconds,
            uptime_b_seconds,
            downtime_a_seconds,
            downtime_b_seconds,
            disconnect_count_a: self.disconnect_count_a,
            disconnect_count_b: self.disconnect_count_b,
            connect_time_sum_a_ms: self.connect_time_sum_a_ms,
            connect_time_sum_b_ms: self.connect_time_sum_b_ms,
            connect_time_count_a: self.connect_time_count_a,
            connect_time_count_b: self.connect_time_count_b,
            total_messages_a: self.total_messages_a,
            total_messages_b: self.total_messages_b,
            total_recovery_time_a_ms: self.total_recovery_time_a_ms,
            total_recovery_time_b_ms: self.total_recovery_time_b_ms,
            recovery_count_a: self.recovery_count_a,
            recovery_count_b: self.recovery_count_b,
            delivery_latency_a_ms: self.get_delivery_latency_a_ms(),
            delivery_latency_b_ms: self.get_delivery_latency_b_ms(),
        }
    }

    pub fn load_totals(&mut self, total_a: u64, total_b: u64) {
        self.total_messages_a = total_a;
        self.total_messages_b = total_b;
    }

    pub fn get_avg_connect_time_a(&self) -> u64 {
        if self.connect_time_count_a > 0 {
            self.connect_time_sum_a_ms / self.connect_time_count_a
        } else {
            0
        }
    }

    pub fn get_avg_connect_time_b(&self) -> u64 {
        if self.connect_time_count_b > 0 {
            self.connect_time_sum_b_ms / self.connect_time_count_b
        } else {
            0
        }
    }

    pub fn get_average_rates(&self) -> (f64, f64) {
        let now = Instant::now();
        let rate_a = Self::rolling_rate(&self.message_samples_a, now);
        let rate_b = Self::rolling_rate(&self.message_samples_b, now);

        (rate_a, rate_b)
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
        fn estimate_window_uptime(
            lifetime_uptime_seconds: u64,
            lifetime_downtime_seconds: u64,
            requested_window_seconds: u64,
        ) -> (u64, u64, u64, f64) {
            if requested_window_seconds == 0 {
                return (0, 0, 0, 0.0);
            }

            let observed_lifetime_seconds =
                lifetime_uptime_seconds.saturating_add(lifetime_downtime_seconds);
            if observed_lifetime_seconds == 0 {
                return (0, 0, 0, 0.0);
            }

            let observed_window_seconds = observed_lifetime_seconds.min(requested_window_seconds);
            let uptime_ratio = lifetime_uptime_seconds as f64 / observed_lifetime_seconds as f64;
            let mut window_uptime_seconds =
                (uptime_ratio * observed_window_seconds as f64).round() as u64;
            window_uptime_seconds = window_uptime_seconds.min(observed_window_seconds);
            let window_downtime_seconds =
                observed_window_seconds.saturating_sub(window_uptime_seconds);
            let uptime_percent = if observed_window_seconds > 0 {
                (window_uptime_seconds as f64 / observed_window_seconds as f64) * 100.0
            } else {
                0.0
            };

            (
                observed_window_seconds,
                window_uptime_seconds,
                window_downtime_seconds,
                uptime_percent,
            )
        }

        let snapshot = self.get_metrics_snapshot();
        let lifetime_observed_a_seconds = snapshot
            .uptime_a_seconds
            .saturating_add(snapshot.downtime_a_seconds);
        let lifetime_observed_b_seconds = snapshot
            .uptime_b_seconds
            .saturating_add(snapshot.downtime_b_seconds);

        let (
            window_observed_a_seconds,
            window_uptime_a_seconds,
            window_downtime_a_seconds,
            window_uptime_a_percent,
        ) = estimate_window_uptime(
            snapshot.uptime_a_seconds,
            snapshot.downtime_a_seconds,
            period_seconds,
        );
        let (
            window_observed_b_seconds,
            window_uptime_b_seconds,
            window_downtime_b_seconds,
            window_uptime_b_percent,
        ) = estimate_window_uptime(
            snapshot.uptime_b_seconds,
            snapshot.downtime_b_seconds,
            period_seconds,
        );

        let lifetime_uptime_a_percent = if lifetime_observed_a_seconds > 0 {
            (snapshot.uptime_a_seconds as f64 / lifetime_observed_a_seconds as f64) * 100.0
        } else {
            0.0
        };
        let lifetime_uptime_b_percent = if lifetime_observed_b_seconds > 0 {
            (snapshot.uptime_b_seconds as f64 / lifetime_observed_b_seconds as f64) * 100.0
        } else {
            0.0
        };

        let rate_a = if snapshot.uptime_a_seconds > 0 {
            snapshot.total_messages_a as f64 / snapshot.uptime_a_seconds as f64
        } else {
            0.0
        };
        let rate_b = if snapshot.uptime_b_seconds > 0 {
            snapshot.total_messages_b as f64 / snapshot.uptime_b_seconds as f64
        } else {
            0.0
        };

        UptimeDetailedStats {
            // Legacy fields retained for compatibility: these now represent the requested window.
            uptime_a_seconds: window_uptime_a_seconds,
            uptime_b_seconds: window_uptime_b_seconds,
            downtime_a_seconds: window_downtime_a_seconds,
            downtime_b_seconds: window_downtime_b_seconds,
            uptime_a_percent: window_uptime_a_percent,
            uptime_b_percent: window_uptime_b_percent,

            window_requested_seconds: period_seconds,
            window_observed_a_seconds,
            window_observed_b_seconds,
            window_uptime_a_seconds,
            window_uptime_b_seconds,
            window_downtime_a_seconds,
            window_downtime_b_seconds,
            window_uptime_a_percent,
            window_uptime_b_percent,

            lifetime_observed_a_seconds,
            lifetime_observed_b_seconds,
            lifetime_uptime_a_seconds: snapshot.uptime_a_seconds,
            lifetime_uptime_b_seconds: snapshot.uptime_b_seconds,
            lifetime_downtime_a_seconds: snapshot.downtime_a_seconds,
            lifetime_downtime_b_seconds: snapshot.downtime_b_seconds,
            lifetime_uptime_a_percent,
            lifetime_uptime_b_percent,

            disconnect_count_a: snapshot.disconnect_count_a,
            disconnect_count_b: snapshot.disconnect_count_b,
            avg_connect_time_a_ms: self.get_avg_connect_time_a(),
            avg_connect_time_b_ms: self.get_avg_connect_time_b(),
            delivery_latency_a_ms: snapshot.delivery_latency_a_ms,
            delivery_latency_b_ms: snapshot.delivery_latency_b_ms,
            mttr_a_ms: self.get_mttr_a_ms(),
            mttr_b_ms: self.get_mttr_b_ms(),
            total_messages_a: snapshot.total_messages_a,
            total_messages_b: snapshot.total_messages_b,
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
    pub downtime_a_seconds: u64,
    pub downtime_b_seconds: u64,
    pub uptime_a_percent: f64,
    pub uptime_b_percent: f64,
    pub window_requested_seconds: u64,
    pub window_observed_a_seconds: u64,
    pub window_observed_b_seconds: u64,
    pub window_uptime_a_seconds: u64,
    pub window_uptime_b_seconds: u64,
    pub window_downtime_a_seconds: u64,
    pub window_downtime_b_seconds: u64,
    pub window_uptime_a_percent: f64,
    pub window_uptime_b_percent: f64,
    pub lifetime_observed_a_seconds: u64,
    pub lifetime_observed_b_seconds: u64,
    pub lifetime_uptime_a_seconds: u64,
    pub lifetime_uptime_b_seconds: u64,
    pub lifetime_downtime_a_seconds: u64,
    pub lifetime_downtime_b_seconds: u64,
    pub lifetime_uptime_a_percent: f64,
    pub lifetime_uptime_b_percent: f64,
    pub disconnect_count_a: u64,
    pub disconnect_count_b: u64,
    pub avg_connect_time_a_ms: u64,
    pub avg_connect_time_b_ms: u64,
    pub delivery_latency_a_ms: f64,
    pub delivery_latency_b_ms: f64,
    pub mttr_a_ms: u64,
    pub mttr_b_ms: u64,
    pub total_messages_a: u64,
    pub total_messages_b: u64,
    pub avg_rate_a: f64,
    pub avg_rate_b: f64,
    pub connected_a: bool,
    pub connected_b: bool,
    pub current_streak_a: f64,
    pub current_streak_b: f64,
}
