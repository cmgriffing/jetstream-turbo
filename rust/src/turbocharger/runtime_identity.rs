use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use serde::Serialize;

const MAX_VALUE_BYTES: usize = 128;
const MAX_EVIDENCE_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticAvailability { Available, Unavailable }

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReleaseIdentityDiagnostics {
    pub availability: DiagnosticAvailability,
    pub identifier: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PreviousTerminationClass {
    None,
    ControlledMemoryExit,
    CgroupOom,
    GlobalOom,
    ApplicationFailure,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PreviousTerminationEvidenceState { Available, Missing, Malformed, Stale }

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PreviousTerminationDiagnostics {
    pub state: PreviousTerminationEvidenceState,
    pub classification: Option<PreviousTerminationClass>,
    pub captured_at_unix_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RuntimeIdentityDiagnostics {
    pub process_started_at_unix_seconds: u64,
    pub release: ReleaseIdentityDiagnostics,
    pub previous_termination: PreviousTerminationDiagnostics,
}

impl RuntimeIdentityDiagnostics {
    pub fn load(release_identifier: Option<&str>, termination_path: &Path) -> Self {
        Self::load_at(release_identifier, termination_path, SystemTime::now())
    }

    fn load_at(release_identifier: Option<&str>, path: &Path, now: SystemTime) -> Self {
        Self {
            process_started_at_unix_seconds: unix_seconds(now),
            release: parse_release_identifier(release_identifier),
            previous_termination: load_previous_termination(path, now),
        }
    }
}

fn parse_release_identifier(value: Option<&str>) -> ReleaseIdentityDiagnostics {
    let identifier = value.map(str::trim).filter(|value| {
        !value.is_empty() && value.len() <= MAX_VALUE_BYTES && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || b"._-".contains(&byte)
        })
    }).map(ToOwned::to_owned);
    ReleaseIdentityDiagnostics {
        availability: if identifier.is_some() { DiagnosticAvailability::Available } else { DiagnosticAvailability::Unavailable },
        identifier,
    }
}

fn load_previous_termination(path: &Path, now: SystemTime) -> PreviousTerminationDiagnostics {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return unavailable(PreviousTerminationEvidenceState::Missing, None);
    };
    let captured_at = value(&contents, "captured_at")
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|timestamp| timestamp.with_timezone(&Utc));
    let classification = value(&contents, "incident_class").and_then(parse_class);
    let (Some(captured_at), Some(classification)) = (captured_at, classification) else {
        return unavailable(PreviousTerminationEvidenceState::Malformed, None);
    };
    let Ok(captured_at_unix_seconds) = captured_at.timestamp().try_into() else {
        return unavailable(PreviousTerminationEvidenceState::Malformed, None);
    };
    if unix_seconds(now).saturating_sub(captured_at_unix_seconds) > MAX_EVIDENCE_AGE.as_secs() {
        return unavailable(PreviousTerminationEvidenceState::Stale, Some(captured_at_unix_seconds));
    }
    PreviousTerminationDiagnostics {
        state: PreviousTerminationEvidenceState::Available,
        classification: Some(classification),
        captured_at_unix_seconds: Some(captured_at_unix_seconds),
    }
}

fn value<'a>(contents: &'a str, key: &str) -> Option<&'a str> {
    contents.lines().find_map(|line| {
        let value = line.strip_prefix(key)?.strip_prefix('=')?.trim();
        (!value.is_empty() && value.len() <= MAX_VALUE_BYTES && value.bytes().all(|b| b.is_ascii_graphic())).then_some(value)
    })
}

fn parse_class(value: &str) -> Option<PreviousTerminationClass> {
    match value {
        "none" => Some(PreviousTerminationClass::None),
        "controlled_memory_exit" => Some(PreviousTerminationClass::ControlledMemoryExit),
        "cgroup_oom" => Some(PreviousTerminationClass::CgroupOom),
        "global_oom" => Some(PreviousTerminationClass::GlobalOom),
        "application_failure" => Some(PreviousTerminationClass::ApplicationFailure),
        _ => None,
    }
}

fn unavailable(state: PreviousTerminationEvidenceState, captured_at_unix_seconds: Option<u64>) -> PreviousTerminationDiagnostics {
    PreviousTerminationDiagnostics { state, classification: None, captured_at_unix_seconds }
}

fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> SystemTime { UNIX_EPOCH + Duration::from_secs(1_800_000_000) }

    fn load(contents: Option<&str>) -> RuntimeIdentityDiagnostics {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("termination.env");
        if let Some(contents) = contents { std::fs::write(&path, contents).unwrap(); }
        RuntimeIdentityDiagnostics::load_at(Some("release-abc"), &path, now())
    }

    #[test]
    fn missing_evidence_is_explicit() {
        assert_eq!(load(None).previous_termination.state, PreviousTerminationEvidenceState::Missing);
    }

    #[test]
    fn valid_evidence_exposes_only_bounded_fields() {
        let identity = load(Some("captured_at=2027-01-15T08:00:00Z\nincident_class=cgroup_oom\nkernel_evidence=did:plc:secret at://secret?token=credential\n"));
        let serialized = serde_json::to_string(&identity).unwrap();
        assert_eq!(identity.previous_termination.classification, Some(PreviousTerminationClass::CgroupOom));
        assert!(!serialized.contains("did:plc"));
        assert!(!serialized.contains("at://"));
        assert!(!serialized.contains("credential"));
    }

    #[test]
    fn malformed_evidence_is_not_partially_exposed() {
        let identity = load(Some("captured_at=bad\nincident_class=global_oom\n"));
        assert_eq!(identity.previous_termination.state, PreviousTerminationEvidenceState::Malformed);
        assert_eq!(identity.previous_termination.classification, None);
    }

    #[test]
    fn stale_evidence_drops_classification() {
        let identity = load(Some("captured_at=2020-01-01T00:00:00Z\nincident_class=application_failure\n"));
        assert_eq!(identity.previous_termination.state, PreviousTerminationEvidenceState::Stale);
        assert_eq!(identity.previous_termination.classification, None);
    }
}
