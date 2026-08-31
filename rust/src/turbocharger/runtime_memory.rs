use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

const KIB: u64 = 1024;
pub const MAX_MONITOR_RECORD_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum WorkloadPhase {
    #[default]
    Startup = 0,
    LiveIngestion = 1,
    Replay = 2,
    Containment = 3,
    DatabaseContention = 4,
    Cleanup = 5,
    Vacuum = 6,
}

impl WorkloadPhase {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::LiveIngestion,
            2 => Self::Replay,
            3 => Self::Containment,
            4 => Self::DatabaseContention,
            5 => Self::Cleanup,
            6 => Self::Vacuum,
            _ => Self::Startup,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::LiveIngestion => "live_ingestion",
            Self::Replay => "replay",
            Self::Containment => "containment",
            Self::DatabaseContention => "database_contention",
            Self::Cleanup => "cleanup",
            Self::Vacuum => "vacuum",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct WorkloadPhaseTracker {
    phase: Arc<AtomicU8>,
}

impl WorkloadPhaseTracker {
    pub fn current(&self) -> WorkloadPhase {
        WorkloadPhase::from_u8(self.phase.load(Ordering::Relaxed))
    }

    pub fn transition(&self, phase: WorkloadPhase) -> bool {
        self.phase.swap(phase as u8, Ordering::Relaxed) != phase as u8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEnvelope {
    pub recovery_bytes: u64,
    pub soft_pressure_bytes: u64,
    pub emergency_bytes: u64,
    pub external_hard_limit_bytes: u64,
    pub host_memory_bytes: u64,
    pub required_host_headroom_bytes: u64,
    pub pressure_confirmation_seconds: u64,
    pub recovery_confirmation_seconds: u64,
}

impl MemoryEnvelope {
    pub fn validate(&self) -> Result<(), MemoryEnvelopeError> {
        if self.recovery_bytes == 0
            || self.soft_pressure_bytes == 0
            || self.emergency_bytes == 0
            || self.external_hard_limit_bytes == 0
            || self.host_memory_bytes == 0
        {
            return Err(MemoryEnvelopeError::NonPositiveThreshold);
        }
        if !(self.recovery_bytes < self.soft_pressure_bytes
            && self.soft_pressure_bytes < self.emergency_bytes
            && self.emergency_bytes < self.external_hard_limit_bytes)
        {
            return Err(MemoryEnvelopeError::InvalidOrdering);
        }
        let available_for_service = self
            .host_memory_bytes
            .checked_sub(self.required_host_headroom_bytes)
            .ok_or(MemoryEnvelopeError::InsufficientHostHeadroom)?;
        if self.external_hard_limit_bytes > available_for_service {
            return Err(MemoryEnvelopeError::InsufficientHostHeadroom);
        }
        if self.pressure_confirmation_seconds == 0 || self.recovery_confirmation_seconds == 0 {
            return Err(MemoryEnvelopeError::NonPositiveInterval);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MemoryEnvelopeError {
    #[error("memory envelope thresholds must be positive")]
    NonPositiveThreshold,
    #[error(
        "memory thresholds must satisfy recovery < soft pressure < emergency < external hard limit"
    )]
    InvalidOrdering,
    #[error("external hard limit does not preserve required host headroom")]
    InsufficientHostHeadroom,
    #[error("memory pressure confirmation intervals must be positive")]
    NonPositiveInterval,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPressureState {
    #[default]
    Normal,
    Reclaiming,
    Throttled,
    Emergency,
    Recovering,
}

impl MemoryPressureState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Reclaiming => "reclaiming",
            Self::Throttled => "throttled",
            Self::Emergency => "emergency",
            Self::Recovering => "recovering",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryPressureActions {
    pub state_changed: bool,
    pub reclaim_caches: bool,
    pub target_permits: usize,
    pub stop_ingestion: bool,
    pub emit_final_snapshot: bool,
}

#[derive(Debug)]
pub struct MemoryPressureCoordinator {
    envelope: MemoryEnvelope,
    state: MemoryPressureState,
    normal_permits: usize,
    current_permits: usize,
    soft_since: Option<Duration>,
    recovery_since: Option<Duration>,
    emergency_snapshot_emitted: bool,
}

impl MemoryPressureCoordinator {
    pub fn new(
        envelope: MemoryEnvelope,
        normal_permits: usize,
    ) -> Result<Self, MemoryEnvelopeError> {
        envelope.validate()?;
        Ok(Self {
            envelope,
            state: MemoryPressureState::Normal,
            normal_permits: normal_permits.max(1),
            current_permits: normal_permits.max(1),
            soft_since: None,
            recovery_since: None,
            emergency_snapshot_emitted: false,
        })
    }

    pub fn state(&self) -> MemoryPressureState {
        self.state
    }

    pub fn envelope(&self) -> MemoryEnvelope {
        self.envelope
    }

    pub fn observe(&mut self, now: Duration, usage_bytes: Option<u64>) -> MemoryPressureActions {
        let Some(usage_bytes) = usage_bytes else {
            return self.actions(false, false, false, false);
        };

        if usage_bytes >= self.envelope.emergency_bytes {
            let changed = self.set_state(MemoryPressureState::Emergency);
            self.current_permits = 0;
            self.soft_since = None;
            self.recovery_since = None;
            let emit_final_snapshot = !self.emergency_snapshot_emitted;
            self.emergency_snapshot_emitted = true;
            return self.actions(changed, true, true, emit_final_snapshot);
        }

        if usage_bytes >= self.envelope.soft_pressure_bytes {
            self.recovery_since = None;
            let soft_since = *self.soft_since.get_or_insert(now);
            if now.saturating_sub(soft_since)
                < Duration::from_secs(self.envelope.pressure_confirmation_seconds)
            {
                return self.actions(false, false, false, false);
            }

            let next_state = match self.state {
                MemoryPressureState::Normal | MemoryPressureState::Recovering => {
                    MemoryPressureState::Reclaiming
                }
                MemoryPressureState::Reclaiming
                | MemoryPressureState::Throttled
                | MemoryPressureState::Emergency => MemoryPressureState::Throttled,
            };
            let changed = self.set_state(next_state);
            self.current_permits = match next_state {
                MemoryPressureState::Reclaiming => (self.normal_permits / 2).max(1),
                MemoryPressureState::Throttled => (self.normal_permits / 4).max(1),
                _ => self.current_permits,
            };
            return self.actions(changed, true, false, false);
        }

        self.soft_since = None;
        if usage_bytes < self.envelope.recovery_bytes && self.state != MemoryPressureState::Normal {
            let recovery_since = *self.recovery_since.get_or_insert(now);
            let changed = self.set_state(MemoryPressureState::Recovering);
            if now.saturating_sub(recovery_since)
                >= Duration::from_secs(self.envelope.recovery_confirmation_seconds)
            {
                self.current_permits = (self.current_permits + 1).min(self.normal_permits);
                self.recovery_since = Some(now);
                if self.current_permits == self.normal_permits {
                    let returned_to_normal = self.set_state(MemoryPressureState::Normal);
                    self.emergency_snapshot_emitted = false;
                    self.recovery_since = None;
                    return self.actions(changed || returned_to_normal, false, false, false);
                }
            }
            return self.actions(changed, false, false, false);
        }

        self.recovery_since = None;
        self.actions(false, false, false, false)
    }

    fn set_state(&mut self, state: MemoryPressureState) -> bool {
        let changed = self.state != state;
        self.state = state;
        changed
    }

    fn actions(
        &self,
        state_changed: bool,
        reclaim_caches: bool,
        stop_ingestion: bool,
        emit_final_snapshot: bool,
    ) -> MemoryPressureActions {
        MemoryPressureActions {
            state_changed,
            reclaim_caches,
            target_permits: self.current_permits,
            stop_ingestion,
            emit_final_snapshot,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessMemoryBreakdown {
    pub rss_bytes: Option<u64>,
    pub anonymous_rss_bytes: Option<u64>,
    pub file_rss_bytes: Option<u64>,
    pub shared_rss_bytes: Option<u64>,
    pub swap_bytes: Option<u64>,
    pub virtual_memory_bytes: Option<u64>,
    pub collection_error: Option<String>,
}

impl ProcessMemoryBreakdown {
    pub fn collect() -> Self {
        match fs::read_to_string("/proc/self/status") {
            Ok(contents) => Self::parse_proc_status(&contents),
            Err(error) => {
                let basic = super::diagnostics::collect_process_memory_diagnostics();
                Self {
                    rss_bytes: basic.rss_bytes,
                    virtual_memory_bytes: basic.virtual_memory_bytes,
                    collection_error: Some(format!(
                        "Linux RSS breakdown unavailable: {error}; basic memory source={}",
                        basic.source
                    )),
                    ..Self::default()
                }
            }
        }
    }

    fn parse_proc_status(contents: &str) -> Self {
        let mut result = Self::default();
        for line in contents.lines() {
            let (field, target) = if line.starts_with("VmRSS:") {
                ("VmRSS", &mut result.rss_bytes)
            } else if line.starts_with("RssAnon:") {
                ("RssAnon", &mut result.anonymous_rss_bytes)
            } else if line.starts_with("RssFile:") {
                ("RssFile", &mut result.file_rss_bytes)
            } else if line.starts_with("RssShmem:") {
                ("RssShmem", &mut result.shared_rss_bytes)
            } else if line.starts_with("VmSwap:") {
                ("VmSwap", &mut result.swap_bytes)
            } else if line.starts_with("VmSize:") {
                ("VmSize", &mut result.virtual_memory_bytes)
            } else {
                continue;
            };
            *target = parse_kib_value(line);
            if target.is_none() {
                result.collection_error = Some(format!("unable to parse {field} from proc status"));
            }
        }
        result
    }
}

// ---------------------------------------------------------------------------
// Cgroup discovery: the process's memory files are located through its actual
// cgroup identity (parsed from /proc/self/cgroup) rather than a fixed mount
// path, with cgroup v1 fallback when no unified-hierarchy line is present.
// ---------------------------------------------------------------------------

/// Which hierarchy the process's cgroup memory files were located in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CgroupHierarchy {
    V2,
    V1,
}

impl CgroupHierarchy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V2 => "v2",
            Self::V1 => "v1",
        }
    }
}

/// Parsed `/proc/self/cgroup` content: the unified-hierarchy path (cgroup v2
/// `0::<path>` line) and the memory-controller path (cgroup v1
/// `12:memory:<path>` line), when present.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CgroupIdentity {
    pub v2_path: Option<String>,
    pub v1_memory_path: Option<String>,
}

/// Parses `/proc/self/cgroup` lines of the form `hierarchy:controllers:path`.
pub fn parse_proc_cgroup(contents: &str) -> CgroupIdentity {
    let mut identity = CgroupIdentity::default();
    for line in contents.lines() {
        let mut fields = line.splitn(3, ':');
        let Some(hierarchy) = fields.next() else {
            continue;
        };
        let Some(controllers) = fields.next() else {
            continue;
        };
        let Some(path) = fields.next() else {
            continue;
        };
        if hierarchy == "0" && controllers.is_empty() {
            identity.v2_path.get_or_insert_with(|| path.to_string());
        } else if controllers.split(',').any(|controller| controller == "memory") {
            identity
                .v1_memory_path
                .get_or_insert_with(|| path.to_string());
        }
    }
    identity
}

/// A single mounted filesystem entry from `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub mount_point: String,
    pub fs_type: String,
    pub super_options: String,
}

/// Parses `/proc/self/mountinfo` into (mount_point, fs_type, super_options)
/// triples, skipping malformed lines.
pub fn parse_mountinfo(contents: &str) -> Vec<MountEntry> {
    contents
        .lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            let after = after.split_whitespace().collect::<Vec<_>>();
            let fs_type = after.first()?.to_string();
            let super_options = after
                .get(2)
                .map(|options| options.to_string())
                .unwrap_or_default();
            // Optional fields separate the fixed prefix from the mount point
            // only in the prefix; the mount point is the 5th fixed field.
            let mount_point = before
                .split_whitespace()
                .nth(4)
                .map(str::to_string)
                .unwrap_or_default();
            Some(MountEntry {
                mount_point,
                fs_type,
                super_options,
            })
        })
        .collect()
}

/// Returns the cgroup v1 memory-controller mount point, if one is present.
pub fn resolve_v1_memory_mount(entries: &[MountEntry]) -> Option<String> {
    entries
        .iter()
        .find(|entry| {
            entry.fs_type == "cgroup"
                && entry
                    .super_options
                    .split(',')
                    .any(|option| option == "memory")
        })
        .map(|entry| entry.mount_point.clone())
}

/// Resolved cgroup memory-file location for this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupMemoryPaths {
    pub hierarchy: CgroupHierarchy,
    /// Directory holding the memory files for this process's cgroup.
    pub memory_dir: String,
    /// Human-readable description of the attempted locations (for errors).
    pub attempted: Vec<String>,
}

/// Resolves the memory-file directory for the process from its parsed cgroup
/// identity. `v2_root` and `memory_mount` are injectable so tests can point at
/// fixture trees; production callers pass `/sys/fs/cgroup` and the v1 memory
/// controller mount resolved from `/proc/self/mountinfo`.
pub fn resolve_cgroup_memory_paths(
    identity: &CgroupIdentity,
    v2_root: &Path,
    memory_mount: Option<&Path>,
) -> Result<CgroupMemoryPaths, String> {
    let mut attempted = Vec::new();

    if let Some(v2_relative) = identity.v2_path.as_deref() {
        let memory_dir = v2_root.join(v2_relative.trim_start_matches('/'));
        let dir_display = memory_dir.display().to_string();
        attempted.push(format!("v2 {dir_display}/memory.current"));
        if memory_dir.join("memory.current").is_file() {
            return Ok(CgroupMemoryPaths {
                hierarchy: CgroupHierarchy::V2,
                memory_dir: dir_display,
                attempted,
            });
        }
    }

    if let Some(v1_relative) = identity.v1_memory_path.as_deref() {
        if let Some(mount) = memory_mount {
            let memory_dir = mount.join(v1_relative.trim_start_matches('/'));
            let dir_display = memory_dir.display().to_string();
            attempted.push(format!("v1 {dir_display}/memory.usage_in_bytes"));
            if memory_dir.join("memory.usage_in_bytes").is_file() {
                return Ok(CgroupMemoryPaths {
                    hierarchy: CgroupHierarchy::V1,
                    memory_dir: dir_display,
                    attempted,
                });
            }
        } else {
            attempted.push(
                "v1 memory controller line present but no cgroup memory mount resolved"
                    .to_string(),
            );
        }
    }

    if attempted.is_empty() {
        attempted.push(
            "no unified (0::<path>) or memory-controller line in /proc/self/cgroup".to_string(),
        );
    }
    Err(attempted.join(", "))
}

/// Resolved cgroup memory paths are cached for the process lifetime: cgroup
/// membership does not change for a running process, so samples reuse them.
static RESOLVED_CGROUP_PATHS: OnceLock<Result<CgroupMemoryPaths, String>> = OnceLock::new();

const PROC_CGROUP_PATH: &str = "/proc/self/cgroup";
const PROC_MOUNTINFO_PATH: &str = "/proc/self/mountinfo";

fn resolve_process_cgroup_paths() -> Result<CgroupMemoryPaths, String> {
    let mut read_errors: Vec<String> = Vec::new();
    let identity = match fs::read_to_string(PROC_CGROUP_PATH) {
        Ok(contents) => parse_proc_cgroup(&contents),
        Err(error) => {
            read_errors.push(format!("unable to read {PROC_CGROUP_PATH}: {error}"));
            CgroupIdentity::default()
        }
    };
    let mount_entries = match fs::read_to_string(PROC_MOUNTINFO_PATH) {
        Ok(contents) => parse_mountinfo(&contents),
        Err(error) => {
            read_errors.push(format!("unable to read {PROC_MOUNTINFO_PATH}: {error}"));
            Vec::new()
        }
    };

    resolve_cgroup_memory_paths(
        &identity,
        Path::new("/sys/fs/cgroup"),
        resolve_v1_memory_mount(&mount_entries)
            .as_deref()
            .map(Path::new),
    )
    .map_err(|error| {
        if read_errors.is_empty() {
            error
        } else {
            format!("{}; {}", read_errors.join("; "), error)
        }
    })
}

fn cgroup_memory_paths() -> &'static Result<CgroupMemoryPaths, String> {
    RESOLVED_CGROUP_PATHS.get_or_init(resolve_process_cgroup_paths)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CgroupMemoryEvents {
    pub low: Option<u64>,
    pub high: Option<u64>,
    pub max: Option<u64>,
    pub oom: Option<u64>,
    pub oom_kill: Option<u64>,
    pub oom_group_kill: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CgroupMemoryDiagnostics {
    /// Which hierarchy the memory files were read from, when resolvable.
    pub hierarchy: Option<CgroupHierarchy>,
    pub current_bytes: Option<u64>,
    pub high_bytes: Option<u64>,
    pub max_bytes: Option<u64>,
    pub high_unlimited: Option<bool>,
    pub max_unlimited: Option<bool>,
    pub pressure_some_avg10: Option<f64>,
    pub pressure_full_avg10: Option<f64>,
    pub events: CgroupMemoryEvents,
    pub collection_error: Option<String>,
}

impl CgroupMemoryDiagnostics {
    pub fn collect() -> Self {
        match cgroup_memory_paths() {
            Ok(paths) => Self::collect_at(paths),
            Err(error) => Self {
                collection_error: Some(error.clone()),
                ..Self::default()
            },
        }
    }

    /// Reads the memory files at the resolved location for the given hierarchy.
    fn collect_at(paths: &CgroupMemoryPaths) -> Self {
        let dir = Path::new(&paths.memory_dir);
        let mut diagnostics = match paths.hierarchy {
            CgroupHierarchy::V2 => Self::collect_v2_from(dir),
            CgroupHierarchy::V1 => Self::collect_v1_from(dir),
        };
        diagnostics.hierarchy = Some(paths.hierarchy);
        if !diagnostics.any_value_available() {
            diagnostics.collection_error = Some(format!(
                "cgroup {} memory files unavailable at {} (attempted: {})",
                paths.hierarchy.as_str(),
                paths.memory_dir,
                paths.attempted.join(", ")
            ));
        }
        diagnostics
    }

    fn any_value_available(&self) -> bool {
        self.current_bytes.is_some()
            || self.high_bytes.is_some()
            || self.max_bytes.is_some()
            || self.high_unlimited.is_some()
            || self.max_unlimited.is_some()
            || self.pressure_some_avg10.is_some()
            || self.pressure_full_avg10.is_some()
            || self.events.low.is_some()
            || self.events.high.is_some()
            || self.events.max.is_some()
            || self.events.oom.is_some()
            || self.events.oom_kill.is_some()
            || self.events.oom_group_kill.is_some()
    }

    fn collect_v2_from(root: &Path) -> Self {
        let current = read_optional_u64(root.join("memory.current"));
        let high = read_limit(root.join("memory.high"));
        let max = read_limit(root.join("memory.max"));
        let events = fs::read_to_string(root.join("memory.events"))
            .ok()
            .map(|contents| parse_cgroup_events(&contents))
            .unwrap_or_default();
        let (pressure_some_avg10, pressure_full_avg10) =
            fs::read_to_string(root.join("memory.pressure"))
                .ok()
                .map(|contents| parse_pressure(&contents))
                .unwrap_or_default();
        Self {
            current_bytes: current,
            high_bytes: high.flatten(),
            max_bytes: max.flatten(),
            high_unlimited: high.map(|value| value.is_none()),
            max_unlimited: max.map(|value| value.is_none()),
            pressure_some_avg10,
            pressure_full_avg10,
            events,
            ..Self::default()
        }
    }

    /// Maps cgroup v1 memory-controller files into the shared diagnostics
    /// shape. Every field stays `Option`-typed and missing files stay
    /// `None`; v1 has no `high` watermark (the soft limit is the closest
    /// equivalent) and its `failcnt` counter maps to the `max` events field.
    fn collect_v1_from(root: &Path) -> Self {
        let current = read_optional_u64(root.join("memory.usage_in_bytes"));
        let max = read_v1_limit(root.join("memory.limit_in_bytes"));
        let high = read_v1_limit(root.join("memory.soft_limit_in_bytes"));
        let failcnt = read_optional_u64(root.join("memory.failcnt"));
        let (pressure_some_avg10, pressure_full_avg10) =
            fs::read_to_string(root.join("memory.pressure"))
                .ok()
                .map(|contents| parse_pressure(&contents))
                .unwrap_or_default();
        let events = CgroupMemoryEvents {
            // v1 counts limit hits in memory.failcnt; this is the closest
            // equivalent of the v2 `max` events counter. v1 exposes no OOM
            // kill counters, so those remain explicitly unavailable.
            max: failcnt,
            ..CgroupMemoryEvents::default()
        };
        Self {
            current_bytes: current,
            high_bytes: high.flatten(),
            max_bytes: max.flatten(),
            high_unlimited: high.map(|value| value.is_none()),
            max_unlimited: max.map(|value| value.is_none()),
            pressure_some_avg10,
            pressure_full_avg10,
            events,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryComponentDiagnostics {
    pub user_cache_entries: usize,
    pub user_cache_entry_limit: usize,
    pub user_cache_evictions: u64,
    pub user_cache_bytes: u64,
    pub user_cache_limit_bytes: u64,
    pub post_cache_entries: usize,
    pub post_cache_entry_limit: usize,
    pub post_cache_evictions: u64,
    pub post_cache_bytes: u64,
    pub post_cache_limit_bytes: u64,
    pub negative_cache_entries: usize,
    pub negative_cache_entry_limit: usize,
    pub negative_cache_evictions: u64,
    pub negative_cache_bytes: u64,
    pub negative_cache_limit_bytes: u64,
    pub coordination_bytes: u64,
    pub input_channel_bytes: u64,
    pub input_channel_limit_bytes: u64,
    pub in_flight_payload_bytes: u64,
    pub in_flight_payload_limit_bytes: u64,
    pub monitor_broadcast_bytes: u64,
    pub monitor_broadcast_limit_bytes: u64,
    pub sqlx_connections: u32,
    pub sqlx_idle_connections: usize,
    pub sqlx_max_connections: u32,
    pub sqlite_cache_bytes_per_connection: u64,
    pub sqlite_mmap_bytes: u64,
    pub sqlite_temp_store: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RuntimeMemorySample {
    pub captured_at_unix_millis: u64,
    pub phase: WorkloadPhase,
    pub pressure_state: MemoryPressureState,
    pub process: ProcessMemoryBreakdown,
    pub cgroup: CgroupMemoryDiagnostics,
    pub components: MemoryComponentDiagnostics,
    pub throughput_per_second: f64,
    pub committed_lag_us: Option<u64>,
    pub checkpoint_ordinal: Option<u64>,
    pub input_occupancy: usize,
    pub queued_batches: usize,
    pub running_batches: usize,
    pub active_permits: usize,
    pub maximum_permits: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRunBaseline {
    pub throughput_per_second: f64,
    pub committed_lag_us: Option<u64>,
    pub bluesky_api_requests: u64,
    pub hydration_complete_ratio: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRunComparison {
    pub candidate: MemoryRunBaseline,
    pub throughput_ratio_to_baseline: Option<f64>,
    pub committed_lag_delta_us: Option<i64>,
    pub bluesky_api_request_ratio_to_baseline: Option<f64>,
    pub hydration_complete_ratio_delta: f64,
}

impl MemoryRunComparison {
    pub fn compare(baseline: &MemoryRunBaseline, candidate: MemoryRunBaseline) -> Self {
        let throughput_ratio_to_baseline = ratio(
            candidate.throughput_per_second,
            baseline.throughput_per_second,
        );
        let committed_lag_delta_us = candidate
            .committed_lag_us
            .zip(baseline.committed_lag_us)
            .map(|(candidate, baseline)| {
                i128::from(candidate)
                    .saturating_sub(i128::from(baseline))
                    .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
            });
        let bluesky_api_request_ratio_to_baseline = ratio(
            candidate.bluesky_api_requests as f64,
            baseline.bluesky_api_requests as f64,
        );
        let hydration_complete_ratio_delta =
            candidate.hydration_complete_ratio - baseline.hydration_complete_ratio;
        Self {
            candidate,
            throughput_ratio_to_baseline,
            committed_lag_delta_us,
            bluesky_api_request_ratio_to_baseline,
            hydration_complete_ratio_delta,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRunConfiguration {
    pub envelope: MemoryEnvelope,
    pub user_cache_entries: usize,
    pub post_cache_entries: usize,
    pub negative_cache_entries: usize,
    pub max_concurrent_requests: usize,
    /// Replay-phase ceiling for concurrent batches; work bounds are evaluated
    /// against the effective maximum of this and `max_concurrent_requests`.
    #[serde(default)]
    pub replay_max_concurrent_batches: usize,
    pub channel_capacity: usize,
    pub max_ingress_event_bytes: usize,
    pub monitor_broadcast_capacity: usize,
    pub in_flight_payload_limit_bytes: u64,
    pub sqlite_max_connections: u32,
    pub sqlite_cache_bytes_per_connection: u64,
    pub database_size_bytes: u64,
    pub event_volume: usize,
    pub settling_window_seconds: u64,
    pub allowed_warmed_growth_bytes: u64,
    pub conservative_working_set_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryAttributionSummary {
    pub observed_peak_bytes: Option<u64>,
    pub confirmed_collector_retained_bytes_after_settle: u64,
    pub attributed_component_peak_bytes: u64,
    pub configured_working_set_ceiling_bytes: u64,
    pub residual_peak_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRunArtifact {
    pub schema_version: u32,
    pub run_id: String,
    pub started_at_unix_millis: u64,
    pub completed_at_unix_millis: u64,
    pub configuration: MemoryRunConfiguration,
    pub baseline: Option<MemoryRunBaseline>,
    pub comparison: Option<MemoryRunComparison>,
    pub samples: Vec<RuntimeMemorySample>,
    pub attribution: MemoryAttributionSummary,
    pub evaluation: MemoryRunEvaluation,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRunEvaluation {
    pub passed: bool,
    pub failures: Vec<String>,
    pub phases_observed: Vec<WorkloadPhase>,
    pub checkpoints_monotonic: bool,
    pub component_bounds_held: bool,
    pub work_bounds_held: bool,
    pub hard_limit_held: bool,
    pub transients_settled: bool,
    pub warmed_slope_held: bool,
}

impl MemoryRunArtifact {
    pub fn evaluate(
        configuration: &MemoryRunConfiguration,
        samples: &[RuntimeMemorySample],
    ) -> MemoryRunEvaluation {
        let mut failures = Vec::new();
        let mut phases_observed = Vec::new();
        for sample in samples {
            if !phases_observed.contains(&sample.phase) {
                phases_observed.push(sample.phase);
            }
        }
        for required in [
            WorkloadPhase::LiveIngestion,
            WorkloadPhase::Replay,
            WorkloadPhase::DatabaseContention,
            WorkloadPhase::Cleanup,
            WorkloadPhase::Vacuum,
        ] {
            if !phases_observed.contains(&required) {
                failures.push(format!(
                    "required phase {} was not observed",
                    required.as_str()
                ));
            }
        }

        let hard_limit_held = samples.iter().all(|sample| {
            sample
                .cgroup
                .current_bytes
                .or(sample.process.rss_bytes)
                .is_none_or(|bytes| bytes < configuration.envelope.external_hard_limit_bytes)
        });
        if !hard_limit_held {
            failures.push("external hard memory limit was crossed".to_string());
        }

        let component_bounds_held = samples.iter().all(|sample| {
            component_bounds_hold(sample)
                && sample.components.sqlx_connections <= configuration.sqlite_max_connections
                && sample.components.sqlx_max_connections <= configuration.sqlite_max_connections
                && sample.components.sqlite_cache_bytes_per_connection
                    <= configuration.sqlite_cache_bytes_per_connection
        });
        if !component_bounds_held {
            failures.push("a bounded component exceeded its byte limit".to_string());
        }

        let work_bounds_held = samples.iter().all(|sample| {
            let effective_max_batches = configuration
                .replay_max_concurrent_batches
                .max(configuration.max_concurrent_requests);
            sample.queued_batches.saturating_add(sample.running_batches)
                <= effective_max_batches
                && sample.active_permits <= sample.maximum_permits
                && sample.maximum_permits <= effective_max_batches
        });
        if !work_bounds_held {
            failures.push("queued or in-flight work exceeded its admission bound".to_string());
        }

        let checkpoints = samples
            .iter()
            .filter_map(|sample| sample.checkpoint_ordinal)
            .collect::<Vec<_>>();
        let checkpoints_monotonic = checkpoints.windows(2).all(|pair| pair[0] <= pair[1]);
        if !checkpoints_monotonic {
            failures.push("durable checkpoint regressed".to_string());
        }

        let transients_settled = [
            WorkloadPhase::Replay,
            WorkloadPhase::DatabaseContention,
            WorkloadPhase::Cleanup,
            WorkloadPhase::Vacuum,
        ]
        .into_iter()
        .all(|phase| {
            samples
                .iter()
                .rev()
                .find(|sample| sample.phase == phase)
                .and_then(sample_usage_bytes)
                .is_none_or(|bytes| bytes < configuration.envelope.recovery_bytes)
        });
        if !transients_settled {
            failures.push("a bounded workload transient did not settle below recovery".to_string());
        }

        let warmed = samples
            .iter()
            .filter(|sample| {
                sample.phase == WorkloadPhase::LiveIngestion
                    && sample.components.user_cache_entries >= configuration.user_cache_entries
                    && sample.components.post_cache_entries >= configuration.post_cache_entries
                    && sample.components.negative_cache_entries
                        >= configuration.negative_cache_entries
            })
            .filter_map(sample_usage_bytes)
            .collect::<Vec<_>>();
        let warmed_slope_held = warmed
            .first()
            .zip(warmed.last())
            .is_none_or(|(first, last)| {
                last.saturating_sub(*first) <= configuration.allowed_warmed_growth_bytes
            });
        if !warmed_slope_held {
            failures.push("warmed memory developed a sustained positive slope".to_string());
        }

        MemoryRunEvaluation {
            passed: failures.is_empty(),
            failures,
            phases_observed,
            checkpoints_monotonic,
            component_bounds_held,
            work_bounds_held,
            hard_limit_held,
            transients_settled,
            warmed_slope_held,
        }
    }

    pub fn attribution(
        configuration: &MemoryRunConfiguration,
        samples: &[RuntimeMemorySample],
    ) -> MemoryAttributionSummary {
        let observed_peak_bytes = samples.iter().filter_map(sample_usage_bytes).max();
        let attributed_component_peak_bytes = samples
            .iter()
            .map(|sample| {
                sample
                    .components
                    .user_cache_bytes
                    .saturating_add(sample.components.post_cache_bytes)
                    .saturating_add(sample.components.negative_cache_bytes)
                    .saturating_add(sample.components.coordination_bytes)
                    .saturating_add(sample.components.input_channel_bytes)
                    .saturating_add(sample.components.in_flight_payload_bytes)
                    .saturating_add(sample.components.monitor_broadcast_bytes)
            })
            .max()
            .unwrap_or(0);
        let configured_working_set_ceiling_bytes = configuration.conservative_working_set_bytes;
        let collector_retained = samples
            .last()
            .map(|sample| sample.components.coordination_bytes)
            .unwrap_or(0);
        MemoryAttributionSummary {
            observed_peak_bytes,
            confirmed_collector_retained_bytes_after_settle: collector_retained,
            attributed_component_peak_bytes,
            configured_working_set_ceiling_bytes,
            residual_peak_bytes: observed_peak_bytes
                .map(|peak| peak.saturating_sub(attributed_component_peak_bytes)),
        }
    }

    pub fn write_json(&self, path: impl AsRef<Path>) -> Result<(), std::io::Error> {
        let payload = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        fs::write(path, payload)
    }
}

fn sample_usage_bytes(sample: &RuntimeMemorySample) -> Option<u64> {
    sample.cgroup.current_bytes.or(sample.process.rss_bytes)
}

fn ratio(candidate: f64, baseline: f64) -> Option<f64> {
    (baseline > 0.0 && baseline.is_finite() && candidate.is_finite())
        .then_some(candidate / baseline)
}

fn component_bounds_hold(sample: &RuntimeMemorySample) -> bool {
    let components = &sample.components;
    components.user_cache_bytes <= components.user_cache_limit_bytes
        && components.user_cache_entries <= components.user_cache_entry_limit
        && components.post_cache_bytes <= components.post_cache_limit_bytes
        && components.post_cache_entries <= components.post_cache_entry_limit
        && components.negative_cache_bytes <= components.negative_cache_limit_bytes
        && components.negative_cache_entries <= components.negative_cache_entry_limit
        && components.input_channel_bytes <= components.input_channel_limit_bytes
        && components.in_flight_payload_bytes <= components.in_flight_payload_limit_bytes
        && components.monitor_broadcast_bytes <= components.monitor_broadcast_limit_bytes
        && components.sqlx_connections <= components.sqlx_max_connections
}

#[derive(Debug)]
pub struct MemoryObserver {
    capacity: usize,
    samples: Mutex<VecDeque<RuntimeMemorySample>>,
}

impl MemoryObserver {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            samples: Mutex::new(VecDeque::with_capacity(capacity.max(1))),
        }
    }

    pub fn record(&self, sample: RuntimeMemorySample) {
        let mut samples = self
            .samples
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if samples.len() == self.capacity {
            samples.pop_front();
        }
        samples.push_back(sample);
    }

    pub fn latest(&self) -> Option<RuntimeMemorySample> {
        self.samples
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .back()
            .cloned()
    }

    pub fn samples(&self) -> Vec<RuntimeMemorySample> {
        self.samples
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.samples
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryIncidentClass {
    ControlledMemoryExit,
    CgroupOom,
    GlobalOom,
    ApplicationFailure,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryIncident {
    pub observed_at_unix_millis: u64,
    pub class: MemoryIncidentClass,
    pub service_result: Option<String>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub cgroup_oom_kill_delta: Option<u64>,
    pub global_oom_evidence: bool,
    pub last_snapshot: Option<RuntimeMemorySample>,
}

impl MemoryIncident {
    pub fn classify(
        controlled_exit: bool,
        service_result: Option<String>,
        exit_code: Option<i32>,
        signal: Option<i32>,
        cgroup_oom_kill_delta: Option<u64>,
        global_oom_evidence: bool,
        last_snapshot: Option<RuntimeMemorySample>,
    ) -> Self {
        let class = if controlled_exit {
            MemoryIncidentClass::ControlledMemoryExit
        } else if cgroup_oom_kill_delta.is_some_and(|delta| delta > 0) {
            MemoryIncidentClass::CgroupOom
        } else if global_oom_evidence {
            MemoryIncidentClass::GlobalOom
        } else if service_result.is_some() || exit_code.is_some() || signal.is_some() {
            MemoryIncidentClass::ApplicationFailure
        } else {
            MemoryIncidentClass::Unknown
        };
        Self {
            observed_at_unix_millis: unix_timestamp_millis(),
            class,
            service_result,
            exit_code,
            signal,
            cgroup_oom_kill_delta,
            global_oom_evidence,
            last_snapshot,
        }
    }
}

fn unix_timestamp_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn parse_kib_value(line: &str) -> Option<u64> {
    line.split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()?
        .checked_mul(KIB)
}

fn read_optional_u64(path: impl AsRef<Path>) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_limit(path: impl AsRef<Path>) -> Option<Option<u64>> {
    let contents = fs::read_to_string(path).ok()?;
    let value = contents.trim();
    if value == "max" {
        Some(None)
    } else {
        value.parse().ok().map(Some)
    }
}

/// Reads a cgroup v1 byte-limit file where `"-1"` means unlimited.
fn read_v1_limit(path: impl AsRef<Path>) -> Option<Option<u64>> {
    let contents = fs::read_to_string(path).ok()?;
    let value = contents.trim();
    if value == "-1" {
        Some(None)
    } else {
        value.parse().ok().map(Some)
    }
}

fn parse_cgroup_events(contents: &str) -> CgroupMemoryEvents {
    let mut events = CgroupMemoryEvents::default();
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        let Some(name) = fields.next() else { continue };
        let value = fields.next().and_then(|value| value.parse().ok());
        match name {
            "low" => events.low = value,
            "high" => events.high = value,
            "max" => events.max = value,
            "oom" => events.oom = value,
            "oom_kill" => events.oom_kill = value,
            "oom_group_kill" => events.oom_group_kill = value,
            _ => {}
        }
    }
    events
}

fn parse_pressure(contents: &str) -> (Option<f64>, Option<f64>) {
    let mut some = None;
    let mut full = None;
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        let target = match fields.next() {
            Some("some") => &mut some,
            Some("full") => &mut full,
            _ => continue,
        };
        *target = fields.find_map(|field| {
            field
                .strip_prefix("avg10=")
                .and_then(|value| value.parse().ok())
        });
    }
    (some, full)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope() -> MemoryEnvelope {
        MemoryEnvelope {
            recovery_bytes: 100,
            soft_pressure_bytes: 200,
            emergency_bytes: 300,
            external_hard_limit_bytes: 400,
            host_memory_bytes: 500,
            required_host_headroom_bytes: 100,
            pressure_confirmation_seconds: 10,
            recovery_confirmation_seconds: 20,
        }
    }

    #[test]
    fn proc_status_parser_preserves_unavailable_dimensions() {
        let sample = ProcessMemoryBreakdown::parse_proc_status("VmRSS:\t10 kB\nVmSwap:\t2 kB\n");
        assert_eq!(sample.rss_bytes, Some(10 * KIB));
        assert_eq!(sample.swap_bytes, Some(2 * KIB));
        assert_eq!(sample.anonymous_rss_bytes, None);
    }

    #[test]
    fn cgroup_event_parser_reads_oom_counters() {
        let events = parse_cgroup_events("low 1\nhigh 2\nmax 3\noom 4\noom_kill 5\n");
        assert_eq!(events.oom, Some(4));
        assert_eq!(events.oom_kill, Some(5));
    }

    fn write_file(dir: &std::path::Path, name: &str, contents: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), contents).unwrap();
    }

    #[test]
    fn proc_cgroup_parser_reads_nested_v2_paths_and_v1_memory_lines() {
        let v2 = parse_proc_cgroup(
            "12:blkio:/user.slice/user-1000.slice\n0::/system.slice/jetstream.service\n",
        );
        assert_eq!(
            v2.v2_path.as_deref(),
            Some("/system.slice/jetstream.service")
        );
        assert_eq!(v2.v1_memory_path, None);

        let v1 = parse_proc_cgroup(
            "12:cpu,cpuacct:/a/b\n11:memory:/system.slice/jetstream.service\n",
        );
        assert_eq!(
            v1.v1_memory_path.as_deref(),
            Some("/system.slice/jetstream.service")
        );
        assert_eq!(v1.v2_path, None);
    }

    #[test]
    fn proc_cgroup_parser_handles_empty_and_absent_files() {
        assert_eq!(parse_proc_cgroup(""), CgroupIdentity::default());
        assert_eq!(
            parse_proc_cgroup("12:cpu:/only-non-memory\n"),
            CgroupIdentity::default()
        );
        assert_eq!(parse_proc_cgroup("malformed-line"), CgroupIdentity::default());
    }

    #[test]
    fn mountinfo_parser_locates_v1_memory_controller_mount() {
        let entries = parse_mountinfo(
            "36 35 0:31 / /sys/fs/cgroup/memory rw,nosuid,nodev - cgroup cgroup rw,memory\n\
             37 35 0:32 / /sys/fs/cgroup/cpu rw,nosuid - cgroup cgroup rw,cpu\n",
        );
        assert_eq!(
            resolve_v1_memory_mount(&entries).as_deref(),
            Some("/sys/fs/cgroup/memory")
        );
        // A v2-only mount table resolves no v1 mount.
        let empty = parse_mountinfo(
            "36 35 0:31 / /sys/fs/cgroup rw,nosuid - cgroup2 cgroup2 rw\n",
        );
        assert_eq!(resolve_v1_memory_mount(&empty), None);
    }

    #[test]
    fn cgroup_paths_resolve_nested_v2_and_v1_layouts_with_injected_roots() {
        let temp = tempfile::tempdir().unwrap();
        let v2_root = temp.path().join("cgroup2");
        let nested = v2_root.join("system.slice/jetstream.service");
        write_file(&nested, "memory.current", "42\n");

        let identity = parse_proc_cgroup("0::/system.slice/jetstream.service\n");
        let resolved =
            resolve_cgroup_memory_paths(&identity, &v2_root, None).unwrap();
        assert_eq!(resolved.hierarchy, CgroupHierarchy::V2);
        assert_eq!(resolved.memory_dir, nested.display().to_string());

        // v1 fallback: memory controller files under the injected mount.
        let v1_mount = temp.path().join("cgroup/memory");
        let v1_dir = v1_mount.join("system.slice/jetstream.service");
        write_file(&v1_dir, "memory.usage_in_bytes", "42\n");
        let identity = parse_proc_cgroup("11:memory:/system.slice/jetstream.service\n");
        let resolved =
            resolve_cgroup_memory_paths(&identity, Path::new("/nonexistent"), Some(&v1_mount))
                .unwrap();
        assert_eq!(resolved.hierarchy, CgroupHierarchy::V1);
        assert_eq!(resolved.memory_dir, v1_dir.display().to_string());
    }

    #[test]
    fn cgroup_path_resolution_failure_names_attempted_locations() {
        let identity = parse_proc_cgroup(
            "11:memory:/system.slice/service\n0::/system.slice/service\n",
        );
        let result =
            resolve_cgroup_memory_paths(&identity, Path::new("/nonexistent-v2"), None);
        let error = result.unwrap_err();
        assert!(error.contains("/nonexistent-v2/system.slice/service/memory.current"));
        assert!(error.contains("no cgroup memory mount resolved"));

        let empty = resolve_cgroup_memory_paths(&CgroupIdentity::default(), Path::new("/x"), None)
            .unwrap_err();
        assert!(empty.contains("no unified"));
    }

    #[test]
    fn cgroup_v2_collector_reads_all_dimensions_from_resolved_path() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("nested/cgroup");
        write_file(&dir, "memory.current", "1024\n");
        write_file(&dir, "memory.high", "2048\n");
        write_file(&dir, "memory.max", "max\n");
        write_file(&dir, "memory.events", "oom 3\noom_kill 1\n");
        write_file(
            &dir,
            "memory.pressure",
            "some avg10=1.25 avg60=0.5 avg300=0.1 total=10\nfull avg10=0.75 avg60=0.2 avg300=0.05 total=5\n",
        );

        let paths = CgroupMemoryPaths {
            hierarchy: CgroupHierarchy::V2,
            memory_dir: dir.display().to_string(),
            attempted: vec!["injected".to_string()],
        };
        let diagnostics = CgroupMemoryDiagnostics::collect_at(&paths);
        assert_eq!(diagnostics.hierarchy, Some(CgroupHierarchy::V2));
        assert_eq!(diagnostics.current_bytes, Some(1024));
        assert_eq!(diagnostics.high_bytes, Some(2048));
        assert_eq!(diagnostics.max_bytes, None);
        assert_eq!(diagnostics.max_unlimited, Some(true));
        assert_eq!(diagnostics.pressure_some_avg10, Some(1.25));
        assert_eq!(diagnostics.events.oom, Some(3));
        assert_eq!(diagnostics.collection_error, None);
    }

    #[test]
    fn cgroup_v1_collector_maps_limit_failcnt_and_pressure() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("memory/service");
        write_file(&dir, "memory.usage_in_bytes", "2048\n");
        write_file(&dir, "memory.limit_in_bytes", "-1\n");
        write_file(&dir, "memory.soft_limit_in_bytes", "4096\n");
        write_file(&dir, "memory.failcnt", "7\n");
        write_file(
            &dir,
            "memory.pressure",
            "some avg10=2.5 avg60=0.5 avg300=0.1 total=10\n",
        );

        let paths = CgroupMemoryPaths {
            hierarchy: CgroupHierarchy::V1,
            memory_dir: dir.display().to_string(),
            attempted: vec!["injected".to_string()],
        };
        let diagnostics = CgroupMemoryDiagnostics::collect_at(&paths);
        assert_eq!(diagnostics.hierarchy, Some(CgroupHierarchy::V1));
        assert_eq!(diagnostics.current_bytes, Some(2048));
        assert_eq!(diagnostics.max_bytes, None, "v1 -1 limit means unlimited");
        assert_eq!(diagnostics.max_unlimited, Some(true));
        assert_eq!(diagnostics.high_bytes, Some(4096));
        assert_eq!(diagnostics.events.max, Some(7), "failcnt maps to max events");
        assert_eq!(diagnostics.events.oom_kill, None, "v1 has no kill counters");
        assert_eq!(diagnostics.pressure_some_avg10, Some(2.5));
        assert_eq!(diagnostics.collection_error, None);
    }

    #[test]
    fn cgroup_collection_failure_remains_explicit_never_zero() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("empty-cgroup");
        std::fs::create_dir_all(&dir).unwrap();

        let paths = CgroupMemoryPaths {
            hierarchy: CgroupHierarchy::V2,
            memory_dir: dir.display().to_string(),
            attempted: vec!["/injected/memory.current".to_string()],
        };
        let diagnostics = CgroupMemoryDiagnostics::collect_at(&paths);
        assert_eq!(diagnostics.current_bytes, None);
        assert_eq!(diagnostics.max_bytes, None);
        assert_eq!(diagnostics.pressure_some_avg10, None);
        let error = diagnostics.collection_error.expect("error must be explicit");
        assert!(error.contains("unavailable"));
        assert!(error.contains("/injected/memory.current"));
    }

    #[test]
    fn pressure_parser_reads_bounded_average_vocabulary() {
        let parsed = parse_pressure(
            "some avg10=1.25 avg60=0.50 avg300=0.10 total=10\nfull avg10=0.75 avg60=0.20 avg300=0.05 total=5\n",
        );
        assert_eq!(parsed, (Some(1.25), Some(0.75)));
    }

    #[test]
    fn memory_envelope_rejects_invalid_ordering() {
        let mut invalid = envelope();
        invalid.soft_pressure_bytes = invalid.recovery_bytes;
        assert_eq!(
            invalid.validate(),
            Err(MemoryEnvelopeError::InvalidOrdering)
        );
    }

    #[test]
    fn memory_envelope_rejects_missing_host_headroom() {
        let mut invalid = envelope();
        invalid.external_hard_limit_bytes = 450;
        assert_eq!(
            invalid.validate(),
            Err(MemoryEnvelopeError::InsufficientHostHeadroom)
        );
    }

    #[test]
    fn observer_retains_only_fixed_capacity() {
        let observer = MemoryObserver::new(2);
        for timestamp in 1..=3 {
            observer.record(RuntimeMemorySample {
                captured_at_unix_millis: timestamp,
                ..RuntimeMemorySample::default()
            });
        }
        let samples = observer.samples();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].captured_at_unix_millis, 2);
    }

    #[test]
    fn pressure_requires_sustained_soft_threshold_before_reclaiming() {
        let mut coordinator = MemoryPressureCoordinator::new(envelope(), 8).unwrap();
        let first = coordinator.observe(Duration::from_secs(1), Some(250));
        let sustained = coordinator.observe(Duration::from_secs(11), Some(250));
        assert!(!first.state_changed);
        assert_eq!(sustained.target_permits, 4);
        assert_eq!(coordinator.state(), MemoryPressureState::Reclaiming);
    }

    #[test]
    fn pressure_derivations_at_nine_permits_remain_at_or_above_one() {
        // The raised permit default derives pressure-state permits by
        // halving (Reclaiming) and quartering (Throttled); both must stay
        // at or above 1 so pressure never fully stalls the pipeline.
        let mut coordinator = MemoryPressureCoordinator::new(envelope(), 9).unwrap();
        coordinator.observe(Duration::from_secs(0), Some(250));
        let reclaimed = coordinator.observe(Duration::from_secs(11), Some(250));
        assert_eq!(reclaimed.target_permits, 4, "9 permits halve to 4");
        assert_eq!(coordinator.state(), MemoryPressureState::Reclaiming);

        let throttled = coordinator.observe(Duration::from_secs(21), Some(250));
        assert_eq!(throttled.target_permits, 2, "9 permits quarter to 2");
        assert_eq!(coordinator.state(), MemoryPressureState::Throttled);
    }

    #[test]
    fn recovery_is_hysteretic_and_restores_permits_gradually() {
        let mut coordinator = MemoryPressureCoordinator::new(envelope(), 4).unwrap();
        coordinator.observe(Duration::from_secs(0), Some(250));
        coordinator.observe(Duration::from_secs(10), Some(250));
        coordinator.observe(Duration::from_secs(11), Some(250));
        assert_eq!(coordinator.state(), MemoryPressureState::Throttled);

        coordinator.observe(Duration::from_secs(12), Some(50));
        let first_restore = coordinator.observe(Duration::from_secs(32), Some(50));
        let second_restore = coordinator.observe(Duration::from_secs(52), Some(50));
        let final_restore = coordinator.observe(Duration::from_secs(72), Some(50));
        assert_eq!(first_restore.target_permits, 2);
        assert_eq!(second_restore.target_permits, 3);
        assert_eq!(final_restore.target_permits, 4);
        assert_eq!(coordinator.state(), MemoryPressureState::Normal);
    }

    #[test]
    fn emergency_snapshot_is_emitted_once_per_incident() {
        let mut coordinator = MemoryPressureCoordinator::new(envelope(), 4).unwrap();
        let first = coordinator.observe(Duration::from_secs(1), Some(350));
        let second = coordinator.observe(Duration::from_secs(2), Some(350));
        assert!(first.stop_ingestion);
        assert!(first.emit_final_snapshot);
        assert!(!second.emit_final_snapshot);
    }

    #[test]
    fn incident_classifier_prioritizes_controlled_and_cgroup_oom_evidence() {
        let controlled = MemoryIncident::classify(true, None, Some(75), None, Some(1), true, None);
        let cgroup = MemoryIncident::classify(false, None, None, Some(9), Some(1), true, None);
        assert_eq!(controlled.class, MemoryIncidentClass::ControlledMemoryExit);
        assert_eq!(cgroup.class, MemoryIncidentClass::CgroupOom);
    }

    #[test]
    fn run_evaluation_rejects_checkpoint_regression_and_unbounded_work() {
        let configuration = MemoryRunConfiguration {
            envelope: envelope(),
            user_cache_entries: 10,
            post_cache_entries: 10,
            negative_cache_entries: 10,
            max_concurrent_requests: 2,
            replay_max_concurrent_batches: 2,
            channel_capacity: 2,
            max_ingress_event_bytes: 10,
            monitor_broadcast_capacity: 2,
            in_flight_payload_limit_bytes: 20,
            sqlite_max_connections: 1,
            sqlite_cache_bytes_per_connection: 10,
            database_size_bytes: 1,
            event_volume: 10,
            settling_window_seconds: 1,
            allowed_warmed_growth_bytes: 10,
            conservative_working_set_bytes: 90,
        };
        let phases = [
            WorkloadPhase::LiveIngestion,
            WorkloadPhase::Replay,
            WorkloadPhase::DatabaseContention,
            WorkloadPhase::Cleanup,
            WorkloadPhase::Vacuum,
        ];
        let mut samples = phases
            .into_iter()
            .enumerate()
            .map(|(index, phase)| RuntimeMemorySample {
                phase,
                checkpoint_ordinal: Some(index as u64),
                maximum_permits: 2,
                components: MemoryComponentDiagnostics {
                    sqlx_max_connections: 1,
                    sqlite_cache_bytes_per_connection: 10,
                    ..MemoryComponentDiagnostics::default()
                },
                process: ProcessMemoryBreakdown {
                    rss_bytes: Some(50),
                    ..ProcessMemoryBreakdown::default()
                },
                ..RuntimeMemorySample::default()
            })
            .collect::<Vec<_>>();
        samples[3].checkpoint_ordinal = Some(1);
        samples[3].queued_batches = 3;

        let evaluation = MemoryRunArtifact::evaluate(&configuration, &samples);
        assert!(!evaluation.passed);
        assert!(!evaluation.checkpoints_monotonic);
        assert!(!evaluation.work_bounds_held);
    }

    #[test]
    fn run_comparison_reports_all_acceptance_dimensions() {
        let baseline = MemoryRunBaseline {
            throughput_per_second: 100.0,
            committed_lag_us: Some(20),
            bluesky_api_requests: 10,
            hydration_complete_ratio: 0.9,
        };
        let comparison = MemoryRunComparison::compare(
            &baseline,
            MemoryRunBaseline {
                throughput_per_second: 110.0,
                committed_lag_us: Some(10),
                bluesky_api_requests: 12,
                hydration_complete_ratio: 0.95,
            },
        );
        assert_eq!(comparison.throughput_ratio_to_baseline, Some(1.1));
        assert_eq!(comparison.committed_lag_delta_us, Some(-10));
        assert_eq!(comparison.bluesky_api_request_ratio_to_baseline, Some(1.2));
        assert!((comparison.hydration_complete_ratio_delta - 0.05).abs() < f64::EPSILON);
    }
}
