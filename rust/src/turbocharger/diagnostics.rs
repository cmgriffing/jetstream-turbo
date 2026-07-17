use crate::turbocharger::progress::{
    PipelineProgressSnapshot, PipelineReadinessState, PipelineStage,
};
use serde::Serialize;
use std::collections::VecDeque;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MEMORY_PEAK_WINDOW_SECS: u64 = 24 * 60 * 60;

// ---------------------------------------------------------------------------
// Public diagnostics structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct HealthStatus {
    pub healthy: bool,
    pub redis_connected: bool,
    pub sqlite_available: bool,
    pub session_count: usize,
    pub diagnostics: HealthDiagnostics,
    pub readiness: ReadinessDiagnostics,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ReadinessDiagnostics {
    pub state: PipelineReadinessState,
    pub stage: Option<PipelineStage>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct HealthDiagnostics {
    pub process_memory: ProcessMemoryDiagnostics,
    pub cache_state: CacheStateDiagnostics,
    pub sqlite_state: SQLiteStateDiagnostics,
    pub not_redis_state: NotRedisStateDiagnostics,
    pub pipeline_progress: PipelineProgressSnapshot,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ProcessMemoryDiagnostics {
    pub pid: u32,
    pub rss_bytes: Option<u64>,
    pub virtual_memory_bytes: Option<u64>,
    pub source: &'static str,
    pub collection_error: Option<String>,
    pub peaks_24h: MemoryPeakDiagnostics,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct MemoryPeakDiagnostics {
    pub window_seconds: u64,
    pub samples_collected: usize,
    pub latest_sample_unix_seconds: Option<u64>,
    pub latest_sample_age_seconds: Option<u64>,
    pub rss_peak_bytes: Option<u64>,
    pub rss_peak_unix_seconds: Option<u64>,
    pub virtual_memory_peak_bytes: Option<u64>,
    pub virtual_memory_peak_unix_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CacheStateDiagnostics {
    pub user_entries: u64,
    pub post_entries: u64,
    pub user_capacity: usize,
    pub post_capacity: usize,
    pub user_hits: u64,
    pub user_misses: u64,
    pub post_hits: u64,
    pub post_misses: u64,
    pub total_requests: u64,
    pub cache_evictions: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SQLiteStateDiagnostics {
    pub available: bool,
    pub db_size_bytes: Option<i64>,
    pub wal_size_bytes: Option<i64>,
    pub page_count: Option<i64>,
    pub page_size_bytes: Option<i64>,
    pub freelist_count: Option<i64>,
    pub cache_size_pages: Option<i64>,
    pub mmap_size_bytes: Option<i64>,
    pub journal_mode: Option<String>,
    pub journal_size_limit_bytes: Option<i64>,
    pub collection_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct NotRedisStateDiagnostics {
    pub connected: bool,
    pub engine: String,
    pub stream_name: String,
    pub stream_length: Option<usize>,
    pub configured_max_length: Option<usize>,
    pub collection_error: Option<String>,
}

// ---------------------------------------------------------------------------
// DiagnosticsCollector
// ---------------------------------------------------------------------------

/// Holds the rolling memory peak window and provides pure assembly of
/// health diagnostics from component snapshots.
pub struct DiagnosticsCollector {
    memory_peak_window: Mutex<MemoryPeakWindow>,
}

impl DiagnosticsCollector {
    pub fn new(window_seconds: u64) -> Self {
        Self {
            memory_peak_window: Mutex::new(MemoryPeakWindow::new(window_seconds)),
        }
    }

    /// Collect raw process memory diagnostics and attach the current 24h peak
    /// data.  This records a new memory sample for peak tracking.
    pub fn capture_memory(&self) -> ProcessMemoryDiagnostics {
        let mut memory = collect_process_memory_diagnostics();
        memory.peaks_24h = {
            let mut peak_window = self
                .memory_peak_window
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let (Some(rss), Some(vmem)) = (memory.rss_bytes, memory.virtual_memory_bytes) {
                let now = unix_timestamp_seconds();
                peak_window.record(MemorySample {
                    captured_at_unix_seconds: now,
                    rss_bytes: rss,
                    virtual_memory_bytes: vmem,
                });
            }
            peak_window.snapshot(unix_timestamp_seconds())
        };
        memory
    }

    /// Pure assembly: combine component snapshots into a `HealthDiagnostics`
    /// value.  Does not mutate any internal state.
    pub fn assemble_health(
        process_memory: ProcessMemoryDiagnostics,
        cache_state: CacheStateDiagnostics,
        sqlite_state: SQLiteStateDiagnostics,
        not_redis_state: NotRedisStateDiagnostics,
        pipeline_progress: PipelineProgressSnapshot,
    ) -> HealthDiagnostics {
        HealthDiagnostics {
            process_memory,
            cache_state,
            sqlite_state,
            not_redis_state,
            pipeline_progress,
        }
    }
}

impl Default for DiagnosticsCollector {
    fn default() -> Self {
        Self::new(MEMORY_PEAK_WINDOW_SECS)
    }
}

// ---------------------------------------------------------------------------
// Derive health
// ---------------------------------------------------------------------------

pub fn derive_health(
    redis_connected: bool,
    sqlite_available: bool,
    session_count: usize,
    progress: &PipelineProgressSnapshot,
    progress_readiness_enabled: bool,
) -> bool {
    redis_connected
        && sqlite_available
        && session_count > 0
        && (!progress_readiness_enabled
            || progress.readiness_state == PipelineReadinessState::Healthy)
}

// ---------------------------------------------------------------------------
// Memory peak window (internal)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct MemorySample {
    captured_at_unix_seconds: u64,
    rss_bytes: u64,
    virtual_memory_bytes: u64,
}

#[derive(Debug)]
struct MemoryPeakWindow {
    window_seconds: u64,
    samples: VecDeque<MemorySample>,
}

impl MemoryPeakWindow {
    fn new(window_seconds: u64) -> Self {
        Self {
            window_seconds,
            samples: VecDeque::new(),
        }
    }

    fn record(&mut self, sample: MemorySample) {
        self.samples.push_back(sample);
        self.trim_old_samples(sample.captured_at_unix_seconds);
    }

    fn snapshot(&mut self, now_unix_seconds: u64) -> MemoryPeakDiagnostics {
        self.trim_old_samples(now_unix_seconds);

        let mut rss_peak: Option<(u64, u64)> = None;
        let mut virtual_peak: Option<(u64, u64)> = None;

        for sample in &self.samples {
            if rss_peak
                .map(|(_, peak_rss)| sample.rss_bytes > peak_rss)
                .unwrap_or(true)
            {
                rss_peak = Some((sample.captured_at_unix_seconds, sample.rss_bytes));
            }
            if virtual_peak
                .map(|(_, peak_virtual)| sample.virtual_memory_bytes > peak_virtual)
                .unwrap_or(true)
            {
                virtual_peak = Some((sample.captured_at_unix_seconds, sample.virtual_memory_bytes));
            }
        }

        let latest_sample_unix_seconds = self
            .samples
            .back()
            .map(|sample| sample.captured_at_unix_seconds);

        MemoryPeakDiagnostics {
            window_seconds: self.window_seconds,
            samples_collected: self.samples.len(),
            latest_sample_unix_seconds,
            latest_sample_age_seconds: latest_sample_unix_seconds
                .map(|captured| now_unix_seconds.saturating_sub(captured)),
            rss_peak_bytes: rss_peak.map(|(_, rss)| rss),
            rss_peak_unix_seconds: rss_peak.map(|(captured, _)| captured),
            virtual_memory_peak_bytes: virtual_peak.map(|(_, vmem)| vmem),
            virtual_memory_peak_unix_seconds: virtual_peak.map(|(captured, _)| captured),
        }
    }

    fn trim_old_samples(&mut self, now_unix_seconds: u64) {
        let window_start = now_unix_seconds.saturating_sub(self.window_seconds);
        while self
            .samples
            .front()
            .map(|sample| sample.captured_at_unix_seconds < window_start)
            .unwrap_or(false)
        {
            self.samples.pop_front();
        }
    }
}

// ---------------------------------------------------------------------------
// Process memory collection (platform-specific, public free function)
// ---------------------------------------------------------------------------

pub fn collect_process_memory_diagnostics() -> ProcessMemoryDiagnostics {
    let pid = std::process::id();

    if let Ok(status_contents) = std::fs::read_to_string("/proc/self/status") {
        if let Some((rss_bytes, virtual_memory_bytes)) =
            parse_proc_status_memory_bytes(&status_contents)
        {
            return ProcessMemoryDiagnostics {
                pid,
                rss_bytes: Some(rss_bytes),
                virtual_memory_bytes: Some(virtual_memory_bytes),
                source: "procfs",
                collection_error: None,
                peaks_24h: MemoryPeakDiagnostics::empty(MEMORY_PEAK_WINDOW_SECS),
            };
        }
    }

    match process_memory_from_ps(pid) {
        Ok((rss_bytes, virtual_memory_bytes)) => ProcessMemoryDiagnostics {
            pid,
            rss_bytes: Some(rss_bytes),
            virtual_memory_bytes: Some(virtual_memory_bytes),
            source: "ps",
            collection_error: None,
            peaks_24h: MemoryPeakDiagnostics::empty(MEMORY_PEAK_WINDOW_SECS),
        },
        Err(error_message) => ProcessMemoryDiagnostics {
            pid,
            rss_bytes: None,
            virtual_memory_bytes: None,
            source: "unavailable",
            collection_error: Some(error_message),
            peaks_24h: MemoryPeakDiagnostics::empty(MEMORY_PEAK_WINDOW_SECS),
        },
    }
}

// ---- MemoryPeakDiagnostics helpers ----

impl MemoryPeakDiagnostics {
    pub fn empty(window_seconds: u64) -> Self {
        Self {
            window_seconds,
            samples_collected: 0,
            latest_sample_unix_seconds: None,
            latest_sample_age_seconds: None,
            rss_peak_bytes: None,
            rss_peak_unix_seconds: None,
            virtual_memory_peak_bytes: None,
            virtual_memory_peak_unix_seconds: None,
        }
    }
}

// ---- Parsing helpers ----

fn unix_timestamp_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs()
}

fn parse_proc_status_memory_bytes(contents: &str) -> Option<(u64, u64)> {
    let mut rss_bytes = None;
    let mut virtual_memory_bytes = None;

    for line in contents.lines() {
        if rss_bytes.is_none() && line.starts_with("VmRSS:") {
            rss_bytes = parse_proc_status_kib_line(line);
        } else if virtual_memory_bytes.is_none() && line.starts_with("VmSize:") {
            virtual_memory_bytes = parse_proc_status_kib_line(line);
        }

        if rss_bytes.is_some() && virtual_memory_bytes.is_some() {
            break;
        }
    }

    match (rss_bytes, virtual_memory_bytes) {
        (Some(rss), Some(vmem)) => Some((rss, vmem)),
        _ => None,
    }
}

fn parse_proc_status_kib_line(line: &str) -> Option<u64> {
    line.split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|value| value.checked_mul(1024))
}

fn process_memory_from_ps(pid: u32) -> Result<(u64, u64), String> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-o", "vsz=", "-p", &pid.to_string()])
        .output()
        .map_err(|e| format!("failed to execute ps: {e}"))?;

    if !output.status.success() {
        return Err(format!("ps exited with status {}", output.status));
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| format!("ps output was not valid UTF-8: {e}"))?;
    parse_ps_memory_output(&stdout).ok_or_else(|| "unable to parse ps memory output".to_string())
}

fn parse_ps_memory_output(stdout: &str) -> Option<(u64, u64)> {
    let mut values = stdout
        .split_whitespace()
        .filter_map(|value| value.parse::<u64>().ok());

    let rss_bytes = values.next()?.checked_mul(1024)?;
    let virtual_memory_bytes = values.next()?.checked_mul(1024)?;
    Some((rss_bytes, virtual_memory_bytes))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turbocharger::{PipelineProgress, ProgressThresholds};

    fn progress_snapshot(valid_ingress: bool) -> PipelineProgressSnapshot {
        let progress = PipelineProgress::new(2, 10);
        if valid_ingress {
            let _ = progress.valid_ingress();
        }
        progress.snapshot(ProgressThresholds {
            startup_grace: if valid_ingress {
                Duration::from_secs(1)
            } else {
                Duration::ZERO
            },
            ingress_idle: Duration::from_secs(1),
            batch_execution: Duration::from_secs(1),
            recovery_successes: 1,
        })
    }

    #[test]
    fn derive_health_requires_redis_connection() {
        assert!(!derive_health(
            false,
            true,
            1,
            &progress_snapshot(true),
            true
        ));
    }

    #[test]
    fn derive_health_requires_sqlite_availability() {
        assert!(!derive_health(
            true,
            false,
            1,
            &progress_snapshot(true),
            true
        ));
    }

    #[test]
    fn derive_health_requires_active_sessions() {
        assert!(!derive_health(
            true,
            true,
            0,
            &progress_snapshot(true),
            true
        ));
    }

    #[test]
    fn derive_health_is_true_when_all_signals_are_healthy() {
        assert!(derive_health(true, true, 1, &progress_snapshot(true), true));
    }

    #[test]
    fn derive_health_honors_progress_rollout_control() {
        let stale = progress_snapshot(false);
        assert!(!derive_health(true, true, 1, &stale, true));
        assert!(derive_health(true, true, 1, &stale, false));
    }

    #[test]
    fn parse_proc_status_memory_bytes_extracts_rss_and_vmsize() {
        let contents = "\
Name:\ttest\n\
VmSize:\t  2048 kB\n\
VmRSS:\t  1024 kB\n";
        let parsed = parse_proc_status_memory_bytes(contents);
        assert_eq!(parsed, Some((1_048_576, 2_097_152)));
    }

    #[test]
    fn parse_ps_memory_output_extracts_values() {
        let parsed = parse_ps_memory_output("12345   67890\n");
        assert_eq!(parsed, Some((12_641_280, 69_519_360)));
    }

    #[test]
    fn memory_peak_window_tracks_high_watermarks_within_window() {
        let mut window = MemoryPeakWindow::new(60);
        window.record(MemorySample {
            captured_at_unix_seconds: 100,
            rss_bytes: 10,
            virtual_memory_bytes: 40,
        });
        window.record(MemorySample {
            captured_at_unix_seconds: 130,
            rss_bytes: 25,
            virtual_memory_bytes: 30,
        });

        let snapshot = window.snapshot(150);
        assert_eq!(snapshot.samples_collected, 2);
        assert_eq!(snapshot.window_seconds, 60);
        assert_eq!(snapshot.latest_sample_unix_seconds, Some(130));
        assert_eq!(snapshot.latest_sample_age_seconds, Some(20));
        assert_eq!(snapshot.rss_peak_bytes, Some(25));
        assert_eq!(snapshot.rss_peak_unix_seconds, Some(130));
        assert_eq!(snapshot.virtual_memory_peak_bytes, Some(40));
        assert_eq!(snapshot.virtual_memory_peak_unix_seconds, Some(100));
    }

    #[test]
    fn memory_peak_window_expires_old_samples() {
        let mut window = MemoryPeakWindow::new(60);
        window.record(MemorySample {
            captured_at_unix_seconds: 10,
            rss_bytes: 10,
            virtual_memory_bytes: 10,
        });
        window.record(MemorySample {
            captured_at_unix_seconds: 70,
            rss_bytes: 20,
            virtual_memory_bytes: 20,
        });
        window.record(MemorySample {
            captured_at_unix_seconds: 75,
            rss_bytes: 30,
            virtual_memory_bytes: 30,
        });

        let first_snapshot = window.snapshot(80);
        assert_eq!(first_snapshot.samples_collected, 2);
        assert_eq!(first_snapshot.rss_peak_bytes, Some(30));
        assert_eq!(first_snapshot.rss_peak_unix_seconds, Some(75));

        let second_snapshot = window.snapshot(140);
        assert_eq!(second_snapshot.samples_collected, 0);
        assert_eq!(second_snapshot.latest_sample_unix_seconds, None);
        assert_eq!(second_snapshot.rss_peak_bytes, None);
        assert_eq!(second_snapshot.virtual_memory_peak_bytes, None);
    }

    #[test]
    fn diagnostics_collector_capture_memory_records_peaks() {
        let collector = DiagnosticsCollector::new(MEMORY_PEAK_WINDOW_SECS);
        let mem = collector.capture_memory();
        assert!(!mem.source.is_empty());
        // On macOS, falls back to ps; on Linux, uses procfs
        assert!(mem.source == "procfs" || mem.source == "ps");
        assert!(mem.peaks_24h.samples_collected > 0);
    }
}
