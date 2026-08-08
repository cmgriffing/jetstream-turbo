use chrono::{DateTime, Utc};
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use tracing::warn;

use crate::stream::StreamId;

/// A structured diagnostic event written to the diagnostic log file.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum DiagnosticEvent {
    /// Emitted when a connection attempt fails (timeout, handshake error, socket error).
    ConnectionAttemptFailed {
        stream_id: StreamId,
        url: String,
        timestamp: DateTime<Utc>,
        error_type: String,
        error_message: String,
        elapsed_ms: u64,
        timeout_seconds: u64,
        attempt_number: u64,
    },
    /// Emitted when an established connection drops.
    Disconnected {
        stream_id: StreamId,
        url: String,
        timestamp: DateTime<Utc>,
        reconnect_reason: String,
    },
    /// Emitted when a connection is successfully established after one or more failures.
    Recovered {
        stream_id: StreamId,
        url: String,
        timestamp: DateTime<Utc>,
        downtime_seconds: u64,
        attempt_count: u64,
    },
}

/// A ring-buffer file logger that writes diagnostic events to a rotating file.
///
/// When the file exceeds `max_bytes`, it is renamed with a `.1` suffix (overwriting
/// any previous `.1` file) and a new file is started. At most two files exist on
/// disk at any time.
pub struct DiagnosticLogger {
    path: PathBuf,
    max_bytes: u64,
    inner: Mutex<Inner>,
}

struct Inner {
    current_size: u64,
}

impl DiagnosticLogger {
    /// Create a new `DiagnosticLogger` writing to `path` with a rotation limit of `max_bytes`.
    pub fn new(path: impl Into<PathBuf>, max_bytes: u64) -> Self {
        let path = path.into();
        let current_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Self {
            path,
            max_bytes,
            inner: Mutex::new(Inner { current_size }),
        }
    }

    /// Append a diagnostic event to the log file, rotating if necessary.
    pub fn log(&self, event: &DiagnosticEvent) {
        let line = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_string());
        let line = format!("{}\n", line);
        let line_bytes = line.len() as u64;

        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(e) => {
                warn!("Diagnostic logger mutex poisoned: {}", e);
                return;
            }
        };

        // Rotate if the new line would push us over the limit.
        if guard.current_size + line_bytes > self.max_bytes && guard.current_size > 0 {
            let rotated = format!("{}.1", self.path.display());
            if let Err(e) = std::fs::rename(&self.path, &rotated) {
                warn!("Failed to rotate diagnostic log: {}", e);
                // If rename fails (e.g. file doesn't exist yet), just reset the size.
            }
            guard.current_size = 0;
        }

        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(line.as_bytes()) {
                    warn!("Failed to write diagnostic event: {}", e);
                } else {
                    guard.current_size += line_bytes;
                }
            }
            Err(e) => {
                warn!("Failed to open diagnostic log file: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::StreamId;

    #[test]
    fn writes_event_to_file_and_rotates() {
        let temp_dir = std::env::temp_dir();
        let log_path = temp_dir.join("test_diag_rotating.log");
        let rotated_path = format!("{}.1", log_path.display());

        // Clean up any leftover files from previous runs.
        let _ = std::fs::remove_file(&log_path);
        let _ = std::fs::remove_file(&rotated_path);

        // Use a tiny max_bytes so a single event triggers rotation.
        let logger = DiagnosticLogger::new(&log_path, 1);

        let event = DiagnosticEvent::ConnectionAttemptFailed {
            stream_id: StreamId::A,
            url: "ws://test.example.com".to_string(),
            timestamp: Utc::now(),
            error_type: "timeout".to_string(),
            error_message: "connection timed out".to_string(),
            elapsed_ms: 15000,
            timeout_seconds: 15,
            attempt_number: 1,
        };

        // Write one event — it fits (current_size is 0).
        logger.log(&event);

        // Write a second event — this should trigger rotation.
        logger.log(&event);

        // The .1 file should now exist (containing the first event).
        let rotated_content = std::fs::read_to_string(&rotated_path).expect("rotated file exists");
        assert!(rotated_content.contains("connection_attempt_failed"));
        assert!(rotated_content.contains("connection timed out"));

        // The current file should contain the second event.
        let current_content = std::fs::read_to_string(&log_path).expect("current log exists");
        assert!(current_content.contains("connection_attempt_failed"));

        // Clean up.
        let _ = std::fs::remove_file(&log_path);
        let _ = std::fs::remove_file(&rotated_path);
    }

    #[test]
    fn writes_disconnect_and_recovery_events() {
        let temp_dir = std::env::temp_dir();
        let log_path = temp_dir.join("test_diag_events.log");
        let rotated_path = format!("{}.1", log_path.display());

        let _ = std::fs::remove_file(&log_path);
        let _ = std::fs::remove_file(&rotated_path);

        let logger = DiagnosticLogger::new(&log_path, 10_000_000);

        logger.log(&DiagnosticEvent::ConnectionAttemptFailed {
            stream_id: StreamId::B,
            url: "wss://example.com".to_string(),
            timestamp: Utc::now(),
            error_type: "handshake".to_string(),
            error_message: "tls handshake failed".to_string(),
            elapsed_ms: 3000,
            timeout_seconds: 15,
            attempt_number: 2,
        });

        logger.log(&DiagnosticEvent::Disconnected {
            stream_id: StreamId::B,
            url: "wss://example.com".to_string(),
            timestamp: Utc::now(),
            reconnect_reason: "peer_close".to_string(),
        });

        logger.log(&DiagnosticEvent::Recovered {
            stream_id: StreamId::B,
            url: "wss://example.com".to_string(),
            timestamp: Utc::now(),
            downtime_seconds: 45,
            attempt_count: 3,
        });

        let content = std::fs::read_to_string(&log_path).expect("log exists");
        assert!(content.contains("connection_attempt_failed"));
        assert!(content.contains("disconnected"));
        assert!(content.contains("recovered"));
        assert!(content.contains("downtime_seconds"));
        assert!(content.contains("attempt_count"));

        let _ = std::fs::remove_file(&log_path);
        let _ = std::fs::remove_file(&rotated_path);
    }
}