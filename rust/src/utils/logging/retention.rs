//! Coordinated, bounded application-file logging. The worker owns both streams
//! and is the only code that creates, rotates, or removes managed segments.
use chrono::Utc;
use fs4::fs_std::FileExt;
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tracing_subscriber::fmt::MakeWriter;
use uuid::Uuid;

const OWNER_LOCK_NAME: &str = ".jetstream-turbo-log-owner.lock";
const MAX_MANAGED_SEGMENTS: usize = 1024;
const IO_RETRY_INTERVAL: Duration = Duration::from_secs(60);

/// File-byte limits include both streams; the queue also has a byte bound:
/// `queue_capacity * max_record_bytes`, plus one record per producer.
#[derive(Debug, Clone)]
pub struct LogPolicy {
    pub max_segment_bytes: u64,
    pub max_total_bytes: u64,
    pub retention_seconds: u64,
    pub queue_capacity: usize,
    pub max_record_bytes: usize,
    pub cleanup_interval: Duration,
}

impl Default for LogPolicy {
    fn default() -> Self {
        Self {
            max_segment_bytes: 100 * 1024 * 1024,
            max_total_bytes: 512 * 1024 * 1024,
            retention_seconds: 7 * 24 * 3600,
            queue_capacity: 256,
            max_record_bytes: 64 * 1024,
            cleanup_interval: Duration::from_secs(60),
        }
    }
}

impl LogPolicy {
    pub fn validate(&self) -> io::Result<()> {
        if self.max_segment_bytes == 0
            || self.max_total_bytes < self.max_segment_bytes
            || self.retention_seconds == 0
            || self.retention_seconds > i64::MAX as u64
            || self.queue_capacity == 0
            || self.max_record_bytes == 0
            || self.max_record_bytes as u64 > self.max_segment_bytes
            || self.cleanup_interval.is_zero()
            || self.cleanup_interval > Duration::from_secs(60)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid application log limits: require positive record <= segment <= total, retention, queue and cleanup <= 60s"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    Main,
    Batches,
}

impl Stream {
    fn name(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Batches => "batches",
        }
    }
}

static GLOBAL_COUNTERS: Mutex<Option<Arc<Counters>>> = Mutex::new(None);

/// Snapshot the process's current sink, including initialization failures.
pub fn log_diagnostics() -> Option<LogDiagnostics> {
    GLOBAL_COUNTERS
        .lock()
        .ok()?
        .as_ref()
        .map(|counters| counters.snapshot())
}

#[derive(Default)]
struct Counters {
    queue_drops: AtomicU64,
    oversize_drops: AtomicU64,
    budget_drops: AtomicU64,
    io_drops: AtomicU64,
    failures: AtomicU64,
    allocated_bytes: AtomicU64,
    main_bytes: AtomicU64,
    batch_bytes: AtomicU64,
    oldest_segment_seconds: AtomicU64,
}

/// Bounded-cardinality sink diagnostics; entity IDs and log contents never appear.
#[derive(Debug, Clone, Serialize)]
pub struct LogDiagnostics {
    pub queue_drops: u64,
    pub oversize_drops: u64,
    pub budget_drops: u64,
    pub io_drops: u64,
    pub retention_failures: u64,
    pub allocated_bytes: u64,
    pub main_bytes: u64,
    pub batch_bytes: u64,
    pub oldest_segment_age_seconds: u64,
}

impl Counters {
    fn snapshot(&self) -> LogDiagnostics {
        LogDiagnostics {
            queue_drops: self.queue_drops.load(Ordering::Relaxed),
            oversize_drops: self.oversize_drops.load(Ordering::Relaxed),
            budget_drops: self.budget_drops.load(Ordering::Relaxed),
            io_drops: self.io_drops.load(Ordering::Relaxed),
            retention_failures: self.failures.load(Ordering::Relaxed),
            allocated_bytes: self.allocated_bytes.load(Ordering::Relaxed),
            main_bytes: self.main_bytes.load(Ordering::Relaxed),
            batch_bytes: self.batch_bytes.load(Ordering::Relaxed),
            oldest_segment_age_seconds: self.oldest_segment_seconds.load(Ordering::Relaxed),
        }
    }

    fn dropped(&self, reason: &'static str, stream: Stream) {
        let counter = match reason {
            "queue" => &self.queue_drops,
            "oversize" => &self.oversize_drops,
            "budget" => &self.budget_drops,
            _ => &self.io_drops,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        metrics::counter!("jetstream_turbo_log_dropped_total", "reason" => reason, "stream" => stream.name()).increment(1);
    }
}

enum Message {
    Record(Stream, Vec<u8>),
    Shutdown,
}

/// A cloneable tracing writer. Each formatted event is buffered only up to the
/// record limit and enqueued without waiting for file I/O or queue capacity.
#[derive(Clone)]
pub struct LogWriter {
    stream: Stream,
    sender: SyncSender<Message>,
    counters: Arc<Counters>,
    max_record_bytes: usize,
}

pub struct EventWriter {
    writer: LogWriter,
    bytes: Vec<u8>,
    oversized: bool,
}

impl<'a> MakeWriter<'a> for LogWriter {
    type Writer = EventWriter;

    fn make_writer(&'a self) -> Self::Writer {
        EventWriter {
            writer: self.clone(),
            bytes: Vec::new(),
            oversized: false,
        }
    }
}

impl Write for EventWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !self.oversized {
            if bytes.len()
                > self
                    .writer
                    .max_record_bytes
                    .saturating_sub(self.bytes.len())
            {
                self.bytes.clear();
                self.oversized = true;
            } else {
                self.bytes.extend_from_slice(bytes);
            }
        }
        // A rejected file event must not prevent independent stdout/journal output.
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for EventWriter {
    fn drop(&mut self) {
        if self.oversized {
            self.writer.counters.dropped("oversize", self.writer.stream);
        } else if !self.bytes.is_empty() {
            let message = Message::Record(self.writer.stream, std::mem::take(&mut self.bytes));
            match self.writer.sender.try_send(message) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    self.writer.counters.dropped("queue", self.writer.stream)
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.writer.counters.dropped("io", self.writer.stream)
                }
            }
        }
    }
}

/// Keep this guard alive until after subscriber shutdown. Dropping it drains
/// queued records and closes both streams before returning.
pub struct LogOwner {
    sender: SyncSender<Message>,
    worker: Option<JoinHandle<()>>,
    counters: Arc<Counters>,
    max_record_bytes: usize,
}

impl LogOwner {
    pub fn start(directory: impl AsRef<Path>, policy: LogPolicy) -> io::Result<Self> {
        policy.validate()?;
        let directory = directory.as_ref();
        let ownership_lock = lock_directory(directory)?;
        let counters = Arc::new(Counters::default());
        if let Ok(mut global) = GLOBAL_COUNTERS.lock() {
            *global = Some(Arc::clone(&counters));
        }
        let mut state = match FileState::open(directory, policy.clone(), Arc::clone(&counters)) {
            Ok(state) => state,
            Err(error) => {
                counters.failures.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("jetstream_turbo_log_retention_failures_total", "operation" => "startup").increment(1);
                return Err(error);
            }
        };
        // Startup cleanup precedes the first possible accepted record.
        state.cleanup(Utc::now().timestamp(), 0)?;
        let (sender, receiver) = mpsc::sync_channel(policy.queue_capacity);
        let max_record_bytes = policy.max_record_bytes;
        let worker = thread::Builder::new()
            .name("application-log-owner".into())
            .spawn(move || {
                // Keep the lock until the final queued write and file close.
                let _ownership_lock = ownership_lock;
                run_worker(state, receiver);
            })?;
        Ok(Self {
            sender,
            worker: Some(worker),
            counters,
            max_record_bytes,
        })
    }

    fn writer(&self, stream: Stream) -> LogWriter {
        LogWriter {
            stream,
            sender: self.sender.clone(),
            counters: Arc::clone(&self.counters),
            max_record_bytes: self.max_record_bytes,
        }
    }

    pub fn main_writer(&self) -> LogWriter {
        self.writer(Stream::Main)
    }

    pub fn batch_writer(&self) -> LogWriter {
        self.writer(Stream::Batches)
    }

    pub fn diagnostics(&self) -> LogDiagnostics {
        self.counters.snapshot()
    }
}

fn lock_directory(directory: &Path) -> io::Result<File> {
    fs::create_dir_all(directory)?;
    let path = directory.join(OWNER_LOCK_NAME);
    if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_symlink()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "application log ownership lock must not be a symlink",
        ));
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    if !file.try_lock_exclusive()? {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "another process owns the application log directory",
        ));
    }
    // Never unlink the lock file: every owner must lock the same inode.
    Ok(file)
}

impl Drop for LogOwner {
    fn drop(&mut self) {
        let _ = self.sender.send(Message::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Segment {
    path: PathBuf,
    stream: Stream,
    first_at: i64,
    bytes: u64,
    file: Option<File>,
}

struct FileState {
    directory: PathBuf,
    policy: LogPolicy,
    segments: Vec<Segment>,
    counters: Arc<Counters>,
    last_failure_report: Option<Instant>,
    retry_at: Option<Instant>,
}

impl FileState {
    fn open(directory: &Path, policy: LogPolicy, counters: Arc<Counters>) -> io::Result<Self> {
        fs::create_dir_all(directory)?;
        let mut state = Self {
            directory: directory.into(),
            policy,
            segments: Vec::new(),
            counters,
            last_failure_report: None,
            retry_at: None,
        };
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            if let Some((stream, first_at)) = parse_owned_name(&name.to_string_lossy()) {
                // Never follow symlinks or remove directories with matching names.
                let metadata = fs::symlink_metadata(entry.path())?;
                if metadata.is_file() {
                    state.segments.push(Segment {
                        path: entry.path(),
                        stream,
                        first_at,
                        bytes: metadata.len(),
                        file: None,
                    });
                    if state.segments.len() > MAX_MANAGED_SEGMENTS {
                        state.cleanup(Utc::now().timestamp(), 0)?;
                    }
                }
            }
        }
        Ok(state)
    }

    fn total_bytes(&self) -> u64 {
        self.segments.iter().map(|segment| segment.bytes).sum()
    }

    fn observe(&self, now: i64) {
        let total = self.total_bytes();
        let age = self
            .segments
            .iter()
            .map(|s| now.saturating_sub(s.first_at).max(0) as u64)
            .max()
            .unwrap_or(0);
        self.counters
            .allocated_bytes
            .store(total, Ordering::Relaxed);
        self.counters
            .oldest_segment_seconds
            .store(age, Ordering::Relaxed);
        metrics::gauge!("jetstream_turbo_log_allocated_bytes").set(total as f64);
        metrics::gauge!("jetstream_turbo_log_oldest_segment_age_seconds").set(age as f64);
        for stream in [Stream::Main, Stream::Batches] {
            let bytes: u64 = self
                .segments
                .iter()
                .filter(|s| s.stream == stream)
                .map(|s| s.bytes)
                .sum();
            let stream_counter = match stream {
                Stream::Main => &self.counters.main_bytes,
                Stream::Batches => &self.counters.batch_bytes,
            };
            stream_counter.store(bytes, Ordering::Relaxed);
            metrics::gauge!("jetstream_turbo_log_stream_bytes", "stream" => stream.name())
                .set(bytes as f64);
        }
    }

    fn failure(&mut self, operation: &'static str) {
        self.counters.failures.fetch_add(1, Ordering::Relaxed);
        metrics::counter!("jetstream_turbo_log_retention_failures_total", "operation" => operation)
            .increment(1);
        if self
            .last_failure_report
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(60))
        {
            // Direct stderr is independent of the subscriber and cannot recurse.
            eprintln!("application file logging degraded: operation={operation}; inspect log-retention counters (stdout/journal remains enabled)");
            self.last_failure_report = Some(Instant::now());
        }
    }

    fn remove(&mut self, index: usize) -> io::Result<()> {
        self.segments[index].file.take();
        match fs::remove_file(&self.segments[index].path) {
            Ok(()) => {
                self.segments.remove(index);
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.segments.remove(index);
                Ok(())
            }
            Err(error) => {
                self.failure("delete");
                Err(error)
            }
        }
    }

    fn cleanup(&mut self, now: i64, incoming: u64) -> io::Result<()> {
        let cutoff = now.saturating_sub(self.policy.retention_seconds as i64);
        let mut first_error = None;
        for index in (0..self.segments.len()).rev() {
            // First-entry time ensures later writes never extend older retention.
            if self.segments[index].first_at <= cutoff {
                if let Err(error) = self.remove(index) {
                    first_error.get_or_insert(error);
                }
            }
        }
        let max_segments = MAX_MANAGED_SEGMENTS - usize::from(incoming > 0);
        while self.total_bytes().saturating_add(incoming) > self.policy.max_total_bytes
            || self.segments.len() > max_segments
        {
            let oldest = self
                .segments
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| (s.file.is_some(), s.first_at, s.path.clone()))
                .map(|(index, _)| index);
            let Some(oldest) = oldest else { break };
            if let Err(error) = self.remove(oldest) {
                first_error.get_or_insert(error);
                break;
            }
        }
        self.observe(now);
        if self.total_bytes().saturating_add(incoming) > self.policy.max_total_bytes
            || self.segments.len() > max_segments
        {
            return Err(first_error.unwrap_or_else(|| {
                io::Error::other("application log allocation cannot fit next record")
            }));
        }
        // Failed expiry is observable but should not disable a fitting healthy stream.
        Ok(())
    }

    fn append(&mut self, stream: Stream, bytes: &[u8], now: i64) {
        if bytes.len() as u64 > self.policy.max_segment_bytes
            || bytes.len() > self.policy.max_record_bytes
        {
            self.counters.dropped("oversize", stream);
            return;
        }
        if self
            .retry_at
            .is_some_and(|retry_at| Instant::now() < retry_at)
        {
            self.counters.dropped("io", stream);
            return;
        }
        for segment in &mut self.segments {
            if segment.stream == stream
                && segment.file.is_some()
                && (segment.first_at.div_euclid(86400) != now.div_euclid(86400)
                    || segment.bytes.saturating_add(bytes.len() as u64)
                        > self.policy.max_segment_bytes)
            {
                segment.file.take();
            }
        }
        if self.cleanup(now, bytes.len() as u64).is_err() {
            self.counters.dropped("budget", stream);
            return;
        }
        let active = self
            .segments
            .iter()
            .position(|segment| segment.stream == stream && segment.file.is_some());
        let index = match active {
            Some(index) => index,
            None => {
                let name = format!(
                    "jetstream-turbo.{}.{now}.{}.log",
                    stream.name(),
                    Uuid::new_v4()
                );
                let path = self.directory.join(name);
                match OpenOptions::new().create_new(true).write(true).open(&path) {
                    Ok(file) => {
                        self.segments.push(Segment {
                            path,
                            stream,
                            first_at: now,
                            bytes: 0,
                            file: Some(file),
                        });
                        metrics::counter!("jetstream_turbo_log_segments_opened_total", "stream" => stream.name()).increment(1);
                        self.segments.len() - 1
                    }
                    Err(_) => {
                        self.retry_at = Some(Instant::now() + IO_RETRY_INTERVAL);
                        self.failure("open");
                        self.counters.dropped("io", stream);
                        return;
                    }
                }
            }
        };
        let segment = &mut self.segments[index];
        if let Some(file) = segment.file.as_mut() {
            if file.write_all(bytes).is_ok() {
                segment.bytes += bytes.len() as u64;
                self.retry_at = None;
            } else {
                // A partial write still consumes the shared budget.
                if let Ok(metadata) = file.metadata() {
                    segment.bytes = metadata.len();
                } else {
                    segment.bytes = self.policy.max_segment_bytes;
                }
                segment.file.take();
                let empty = segment.bytes == 0;
                self.retry_at = Some(Instant::now() + IO_RETRY_INTERVAL);
                self.failure("write");
                self.counters.dropped("io", stream);
                if empty {
                    let _ = self.remove(index);
                }
            }
        }
        self.observe(now);
    }
}

fn parse_owned_name(name: &str) -> Option<(Stream, i64)> {
    let parts: Vec<_> = name.split('.').collect();
    if parts.len() != 5 || parts[0] != "jetstream-turbo" || parts[4] != "log" {
        return None;
    }
    let stream = match parts[1] {
        "main" => Stream::Main,
        "batches" => Stream::Batches,
        _ => return None,
    };
    let timestamp: i64 = parts[2].parse().ok()?;
    if timestamp < 0
        || timestamp.to_string() != parts[2]
        || Uuid::parse_str(parts[3]).ok()?.hyphenated().to_string() != parts[3]
    {
        return None;
    }
    Some((stream, timestamp))
}

fn run_worker(mut state: FileState, receiver: Receiver<Message>) {
    let mut next_cleanup = Instant::now() + state.policy.cleanup_interval;
    loop {
        let timeout = next_cleanup.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(timeout) {
            Ok(Message::Record(stream, bytes)) => {
                state.append(stream, &bytes, Utc::now().timestamp())
            }
            Ok(Message::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= next_cleanup {
            let _ = state.cleanup(Utc::now().timestamp(), 0);
            next_cleanup = Instant::now() + state.policy.cleanup_interval;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> LogPolicy {
        LogPolicy {
            max_segment_bytes: 10,
            max_total_bytes: 25,
            retention_seconds: 100,
            queue_capacity: 2,
            max_record_bytes: 10,
            cleanup_interval: Duration::from_millis(10),
        }
    }

    fn state(directory: &Path) -> FileState {
        FileState::open(directory, policy(), Arc::new(Counters::default())).unwrap()
    }

    fn managed_paths(directory: &Path) -> Vec<PathBuf> {
        fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| parse_owned_name(path.file_name().unwrap().to_str().unwrap()).is_some())
            .collect()
    }

    #[test]
    fn directory_owner_is_exclusive_across_processes_and_released_after_shutdown() {
        const PROBE_DIRECTORY: &str = "JETSTREAM_LOG_LOCK_TEST_DIRECTORY";
        if let Some(directory) = std::env::var_os(PROBE_DIRECTORY) {
            let error = LogOwner::start(PathBuf::from(directory), policy())
                .err()
                .expect("child must not acquire the parent's directory");
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let owner = LogOwner::start(dir.path(), policy()).unwrap();
        let error = LogOwner::start(dir.path(), policy())
            .err()
            .expect("second owner must not acquire the directory");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("utils::logging::retention::tests::directory_owner_is_exclusive_across_processes_and_released_after_shutdown")
            .env(PROBE_DIRECTORY, dir.path())
            .output()
            .unwrap();
        assert!(
            child.status.success() && String::from_utf8_lossy(&child.stdout).contains("1 passed"),
            "lock probe failed: stdout={} stderr={}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(dir.path().join(OWNER_LOCK_NAME).is_file());
        drop(owner);
        drop(LogOwner::start(dir.path(), policy()).unwrap());
    }

    #[test]
    fn write_failure_removes_empty_segment_and_bounds_retries_until_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path());
        let path = dir
            .path()
            .join(format!("jetstream-turbo.main.1000.{}.log", Uuid::new_v4()));
        fs::write(&path, []).unwrap();
        state.segments.push(Segment {
            file: Some(File::open(&path).unwrap()),
            path,
            stream: Stream::Main,
            first_at: 1000,
            bytes: 0,
        });
        // A read-only descriptor deterministically rejects writes on every
        // supported platform without quotas, root privileges, or a full disk.
        for _ in 0..40 {
            state.append(Stream::Main, b"hello", 1000);
        }
        assert!(state.segments.is_empty());
        assert!(managed_paths(dir.path()).is_empty());
        assert_eq!(state.counters.snapshot().retention_failures, 1);
        assert_eq!(state.counters.snapshot().io_drops, 40);
        state.retry_at = Some(Instant::now());
        state.append(Stream::Main, b"healthy", 1001);
        assert_eq!(state.total_bytes(), 7);
        assert!(state.retry_at.is_none());
    }

    #[test]
    fn open_failure_bounds_retries_then_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path());
        state.directory = dir.path().join("missing-parent");
        for _ in 0..40 {
            state.append(Stream::Main, b"hello", 1000);
        }
        assert_eq!(state.counters.snapshot().retention_failures, 1);
        assert_eq!(state.counters.snapshot().io_drops, 40);
        assert!(state.segments.is_empty());
        state.directory = dir.path().into();
        state.retry_at = Some(Instant::now());
        state.append(Stream::Main, b"healthy", 1001);
        assert_eq!(state.total_bytes(), 7);
    }

    #[test]
    fn segment_count_is_bounded_even_for_zero_byte_files() {
        let dir = tempfile::tempdir().unwrap();
        let now = Utc::now().timestamp();
        for _ in 0..MAX_MANAGED_SEGMENTS + 2 {
            let name = format!("jetstream-turbo.main.{now}.{}.log", Uuid::new_v4());
            fs::write(dir.path().join(name), []).unwrap();
        }
        let mut state = state(dir.path());
        assert_eq!(state.segments.len(), MAX_MANAGED_SEGMENTS);
        state.append(Stream::Main, b"hello", now);
        assert_eq!(state.segments.len(), MAX_MANAGED_SEGMENTS);
        assert_eq!(managed_paths(dir.path()).len(), MAX_MANAGED_SEGMENTS);
        assert_eq!(state.total_bytes(), 5);
    }

    #[test]
    fn rotates_at_size_and_utc_boundary_with_shared_stream_budget() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path());
        state.append(Stream::Main, b"12345678", 86399);
        state.append(Stream::Main, b"1234", 86399);
        state.append(Stream::Batches, b"12345678", 86399);
        assert_eq!(state.segments.len(), 3);
        state.append(Stream::Main, b"12345678", 86400);
        assert!(state.total_bytes() <= 25);
        assert!(!state
            .segments
            .iter()
            .any(|s| s.first_at == 86399 && s.file.is_some() && s.stream == Stream::Main));
        assert!(state.segments.iter().any(|s| s.first_at == 86400));
    }

    #[test]
    fn age_uses_first_entry_and_cleanup_needs_no_append() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path());
        state.append(Stream::Main, b"1234", 1000);
        state.append(Stream::Main, b"1234", 1099);
        state.cleanup(1100, 0).unwrap();
        assert!(state.segments.is_empty());
        assert!(managed_paths(dir.path()).is_empty());
    }

    #[test]
    fn startup_owns_only_exact_regular_segment_names() {
        let dir = tempfile::tempdir().unwrap();
        let owned = format!("jetstream-turbo.main.1000.{}.log", Uuid::new_v4());
        fs::write(dir.path().join(owned), "old").unwrap();
        for name in [
            "jetstream-turbo.log",
            "jetstream-turbo.main.1000.invalid.log",
            "other.log",
        ] {
            fs::write(dir.path().join(name), "keep").unwrap();
        }
        let mut state = state(dir.path());
        state.cleanup(1100, 0).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);
    }

    #[test]
    fn failed_deletion_rejects_append_without_budget_growth() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path());
        state.append(Stream::Main, b"1234567890", 1000);
        state.append(Stream::Batches, b"1234567890", 1000);
        for segment in &mut state.segments {
            segment.file.take();
            fs::remove_file(&segment.path).unwrap();
            fs::create_dir(&segment.path).unwrap();
        }
        state.append(Stream::Main, b"1234567890", 1001);
        assert_eq!(state.total_bytes(), 20);
        assert_eq!(state.counters.snapshot().budget_drops, 1);
        assert_eq!(state.counters.snapshot().retention_failures, 1);
    }

    #[test]
    fn opening_failure_and_oversize_are_observable() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path());
        state.append(Stream::Main, b"12345678901", 1000);
        state.directory = dir.path().join("missing-parent");
        state.append(Stream::Batches, b"123", 1000);
        let snapshot = state.counters.snapshot();
        assert_eq!(snapshot.oversize_drops, 1);
        assert_eq!(snapshot.io_drops, 1);
        assert_eq!(snapshot.retention_failures, 1);
        assert_eq!(snapshot.allocated_bytes, 0);
    }

    #[test]
    fn producers_bound_record_size_and_never_wait_for_queue_space() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let counters = Arc::new(Counters::default());
        let writer = LogWriter {
            stream: Stream::Main,
            sender,
            counters: Arc::clone(&counters),
            max_record_bytes: 10,
        };
        for _ in 0..3 {
            let mut record = writer.make_writer();
            record.write_all(b"1234567890").unwrap();
        }
        let mut record = writer.make_writer();
        record.write_all(b"12345678901").unwrap();
        assert!(record.bytes.is_empty());
        drop(record);
        assert_eq!(counters.snapshot().queue_drops, 2);
        assert_eq!(counters.snapshot().oversize_drops, 1);
    }

    #[test]
    fn rotation_open_failure_keeps_existing_segment_and_drops_next_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path());
        state.append(Stream::Main, b"1234567890", 1000);
        state.directory = dir.path().join("missing-parent");
        state.append(Stream::Main, b"1", 1001);
        assert_eq!(state.total_bytes(), 10);
        assert_eq!(state.segments.len(), 1);
        assert!(state.segments[0].file.is_none());
        assert_eq!(state.counters.snapshot().io_drops, 1);
    }

    #[cfg(unix)]
    #[test]
    fn matching_symlink_never_becomes_owned() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("unrelated.txt");
        fs::write(&target, "keep").unwrap();
        let link = dir
            .path()
            .join(format!("jetstream-turbo.main.1.{}.log", Uuid::new_v4()));
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut state = state(dir.path());
        state.cleanup(200, 0).unwrap();
        assert!(link.is_symlink());
        assert_eq!(fs::read(target).unwrap(), b"keep");
    }

    #[test]
    fn scheduled_cleanup_expires_a_quiet_active_segment() {
        let dir = tempfile::tempdir().unwrap();
        let mut policy = policy();
        policy.retention_seconds = 1;
        let owner = LogOwner::start(dir.path(), policy).unwrap();
        let writer = owner.main_writer();
        let mut event = writer.make_writer();
        event.write_all(b"hello").unwrap();
        drop(event);
        let deadline = Instant::now() + Duration::from_secs(3);
        while owner.diagnostics().allocated_bytes == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(owner.diagnostics().allocated_bytes, 5);
        while owner.diagnostics().allocated_bytes > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(owner.diagnostics().allocated_bytes, 0);
        assert!(managed_paths(dir.path()).is_empty());
    }

    #[test]
    fn worker_expires_segments_while_quiet_and_guard_drains() {
        let dir = tempfile::tempdir().unwrap();
        let old = format!("jetstream-turbo.main.1.{}.log", Uuid::new_v4());
        fs::write(dir.path().join(old), "expired").unwrap();
        let owner = LogOwner::start(dir.path(), policy()).unwrap();
        assert!(managed_paths(dir.path()).is_empty());
        let writer = owner.main_writer();
        let mut event = writer.make_writer();
        event.write_all(b"hello").unwrap();
        drop(event);
        drop(owner);
        let files = managed_paths(dir.path());
        assert_eq!(files.len(), 1);
        assert_eq!(fs::read(&files[0]).unwrap(), b"hello");
    }
}
