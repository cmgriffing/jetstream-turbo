//! SQLite-backed incident ledger storage.
//!
//! Schema additions are additive; incidents and their ordered events are
//! written transactionally. The ledger stores only bounded, sanitized
//! operational evidence.

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use sqlx::SqlitePool;

use super::{
    IncidentEvent, IncidentEventType, IncidentId, IncidentState, IncidentSummary, IncidentTrigger,
    MonitorIdentity,
};

/// Retention default in days for terminal-state incidents.
pub const DEFAULT_INCIDENT_RETENTION_DAYS: u64 = 90;

/// Bounded filters accepted by the incident list query.
#[derive(Debug, Clone, Default)]
pub struct IncidentFilter {
    pub stream_id: Option<String>,
    pub state: Option<IncidentState>,
    pub trigger: Option<IncidentTrigger>,
    /// Incidents detected at or after this time.
    pub detected_from: Option<DateTime<Utc>>,
    /// Incidents detected at or before this time.
    pub detected_to: Option<DateTime<Utc>>,
    /// Minimum total silence duration in milliseconds.
    pub min_silence_ms: Option<u64>,
}

/// A page of incident summaries plus the opaque cursor for the next page.
#[derive(Debug, Clone)]
pub struct IncidentPage {
    pub incidents: Vec<IncidentSummary>,
    pub next_cursor: Option<String>,
}

/// A page of incident events plus the next sequence cursor.
#[derive(Debug, Clone)]
pub struct EventPage {
    pub events: Vec<IncidentEvent>,
    pub next_cursor: Option<i64>,
}

type EventRow = (
    i64,
    String,
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

#[derive(Debug, sqlx::FromRow)]
struct SummaryRow {
    id: String,
    stream_id: String,
    state: String,
    trigger: String,
    last_useful_record_at: Option<String>,
    detected_at: String,
    resolved_at: Option<String>,
    transport_recovered_at: Option<String>,
    total_silence_ms: Option<i64>,
    detected_recovery_ms: Option<i64>,
    reconnect_attempts: i64,
    connection_epoch: i64,
    observation_complete: i64,
    monitor_process_epoch: String,
    monitor_release: String,
    created_at: String,
    updated_at: String,
}

fn state_str(state: IncidentState) -> &'static str {
    match state {
        IncidentState::Open => "open",
        IncidentState::Resolved => "resolved",
        IncidentState::Incomplete => "incomplete",
    }
}

fn trigger_str(trigger: IncidentTrigger) -> &'static str {
    match trigger {
        IncidentTrigger::DeliveryIdle => "delivery_idle",
        IncidentTrigger::TransportLoss => "transport_loss",
        IncidentTrigger::DuplicateDelivery => "duplicate_delivery",
        IncidentTrigger::OrdinalGap => "ordinal_gap",
    }
}

fn parse_state(value: &str) -> Result<IncidentState> {
    match value {
        "open" => Ok(IncidentState::Open),
        "resolved" => Ok(IncidentState::Resolved),
        "incomplete" => Ok(IncidentState::Incomplete),
        other => Err(anyhow::anyhow!("unknown incident state {}", other)),
    }
}

fn parse_trigger(value: &str) -> Result<IncidentTrigger> {
    match value {
        "delivery_idle" => Ok(IncidentTrigger::DeliveryIdle),
        "transport_loss" => Ok(IncidentTrigger::TransportLoss),
        "duplicate_delivery" => Ok(IncidentTrigger::DuplicateDelivery),
        "ordinal_gap" => Ok(IncidentTrigger::OrdinalGap),
        other => Err(anyhow::anyhow!("unknown incident trigger {}", other)),
    }
}

fn parse_time(value: Option<String>) -> Result<Option<DateTime<Utc>>> {
    match value {
        None => Ok(None),
        Some(v) => DateTime::parse_from_rfc3339(&v)
            .map(|d| Some(d.with_timezone(&Utc)))
            .map_err(|e| anyhow::anyhow!("invalid timestamp: {}", e)),
    }
}

fn rfc3339(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn row_to_summary(row: SummaryRow) -> Result<IncidentSummary> {
    let SummaryRow {
        id,
        stream_id,
        state,
        trigger,
        last_useful_record_at,
        detected_at,
        resolved_at,
        transport_recovered_at,
        total_silence_ms,
        detected_recovery_ms,
        reconnect_attempts,
        connection_epoch,
        observation_complete,
        monitor_process_epoch,
        monitor_release,
        created_at,
        updated_at,
    } = row;
    Ok(IncidentSummary {
        id: IncidentId::from_string(id).ok_or_else(|| anyhow::anyhow!("invalid incident id"))?,
        stream_id,
        state: parse_state(&state)?,
        trigger: parse_trigger(&trigger)?,
        last_useful_record_at: parse_time(last_useful_record_at)?,
        detected_at: parse_time(Some(detected_at))?
            .ok_or_else(|| anyhow::anyhow!("missing detected_at"))?,
        resolved_at: parse_time(resolved_at)?,
        transport_recovered_at: parse_time(transport_recovered_at)?,
        total_silence_ms: total_silence_ms.map(|v| v.max(0) as u64),
        detected_recovery_ms: detected_recovery_ms.map(|v| v.max(0) as u64),
        reconnect_attempts: reconnect_attempts.max(0) as u64,
        connection_epoch: connection_epoch.max(0) as u64,
        observation_complete: observation_complete != 0,
        monitor_process_epoch,
        monitor_release,
        created_at: parse_time(Some(created_at))?
            .ok_or_else(|| anyhow::anyhow!("missing created_at"))?,
        updated_at: parse_time(Some(updated_at))?
            .ok_or_else(|| anyhow::anyhow!("missing updated_at"))?,
    })
}

/// Opaque incident cursor: `v1:<detected_at>|<id>`, safe because neither
/// component ever contains `|`.
fn encode_incident_cursor(detected_at: &str, id: &str) -> String {
    format!("v1:{}|{}", detected_at, id)
}

/// Validate an opaque incident cursor, returning a bounded error message.
pub fn validate_incident_cursor(cursor: &str) -> Result<(), String> {
    match decode_incident_cursor(cursor) {
        Ok(_) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn decode_incident_cursor(cursor: &str) -> Result<(String, String)> {
    let rest = cursor
        .strip_prefix("v1:")
        .ok_or_else(|| anyhow::anyhow!("unsupported cursor version"))?;
    let (time, id) = rest
        .rsplit_once('|')
        .ok_or_else(|| anyhow::anyhow!("invalid cursor shape"))?;
    if id.is_empty() || id.len() != 26 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(anyhow::anyhow!("invalid cursor id"));
    }
    DateTime::parse_from_rfc3339(time).map_err(|_| anyhow::anyhow!("invalid cursor timestamp"))?;
    Ok((time.to_string(), id.to_string()))
}

/// Durable incident ledger storage.
pub struct IncidentStore {
    pool: SqlitePool,
}

impl IncidentStore {
    /// Attach to an existing monitor database pool and apply additive migrations.
    pub async fn new(pool: SqlitePool) -> Result<Self> {
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    async fn migrate(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS monitor_incidents (
                id TEXT PRIMARY KEY,
                stream_id TEXT NOT NULL,
                state TEXT NOT NULL,
                trigger TEXT NOT NULL,
                last_useful_record_at TEXT,
                detected_at TEXT NOT NULL,
                resolved_at TEXT,
                transport_recovered_at TEXT,
                total_silence_ms INTEGER,
                detected_recovery_ms INTEGER,
                reconnect_attempts INTEGER NOT NULL DEFAULT 0,
                connection_epoch INTEGER NOT NULL DEFAULT 0,
                observation_complete INTEGER NOT NULL DEFAULT 1,
                monitor_process_epoch TEXT NOT NULL,
                monitor_release TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_monitor_incidents_keyset
                ON monitor_incidents (detected_at DESC, id DESC)
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_monitor_incidents_stream ON monitor_incidents (stream_id)",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_monitor_incidents_state ON monitor_incidents (state)",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS monitor_incident_events (
                incident_id TEXT NOT NULL REFERENCES monitor_incidents(id) ON DELETE CASCADE,
                sequence INTEGER NOT NULL,
                event_type TEXT NOT NULL,
                occurred_at TEXT NOT NULL,
                reason TEXT,
                attempt_ordinal INTEGER,
                scheduled_delay_ms INTEGER,
                evidence TEXT,
                PRIMARY KEY (incident_id, sequence)
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_monitor_incident_events_unique \
             ON monitor_incident_events (incident_id, sequence)",
        )
        .execute(&self.pool)
        .await
        .ok();

        // Additive migration: sanitize evidence JSON on threshold events.
        let event_columns = sqlx::query("PRAGMA table_info(monitor_incident_events)")
            .fetch_all(&self.pool)
            .await?;
        let has_evidence = event_columns
            .iter()
            .any(|row| {
                use sqlx::Row;
                row.try_get::<String, _>("name").is_ok_and(|n| n == "evidence")
            });
        if !has_evidence {
            sqlx::query("ALTER TABLE monitor_incident_events ADD COLUMN evidence TEXT")
                .execute(&self.pool)
                .await?;
        }

        Ok(())
    }

    /// Open a new incident; duplicate IDs are ignored idempotently.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_incident(
        &self,
        incident_id: &IncidentId,
        stream_id: &str,
        trigger: IncidentTrigger,
        detected_at: DateTime<Utc>,
        last_useful_record_at: Option<DateTime<Utc>>,
        connection_epoch: u64,
        identity: &MonitorIdentity,
    ) -> Result<()> {
        let now = rfc3339(Utc::now());
        sqlx::query(
            r#"
            INSERT OR IGNORE INTO monitor_incidents (
                id, stream_id, state, trigger, last_useful_record_at, detected_at,
                reconnect_attempts, connection_epoch, observation_complete,
                monitor_process_epoch, monitor_release, created_at, updated_at
            )
            VALUES (?, ?, 'open', ?, ?, ?, 0, ?, 1, ?, ?, ?, ?)
            "#,
        )
        .bind(incident_id.as_str())
        .bind(stream_id)
        .bind(trigger_str(trigger))
        .bind(last_useful_record_at.map(rfc3339))
        .bind(rfc3339(detected_at))
        .bind(connection_epoch as i64)
        .bind(&identity.process_epoch)
        .bind(&identity.release)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Append one ordered event and update the affected summary fields
    /// transactionally. Duplicate sequences are rejected by the unique index.
    pub async fn append_event(&self, incident_id: &IncidentId, event: IncidentEvent) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let event_type_str = serde_json::to_string(&event.event_type)?
            .trim_matches('"')
            .to_string();
        sqlx::query(
            r#"
            INSERT INTO monitor_incident_events (
                incident_id, sequence, event_type, occurred_at, reason,
                attempt_ordinal, scheduled_delay_ms, evidence
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(incident_id.as_str())
        .bind(event.sequence)
        .bind(&event_type_str)
        .bind(rfc3339(event.occurred_at))
        .bind(event.reason.as_deref())
        .bind(event.attempt_ordinal.map(|v| v as i64))
        .bind(event.scheduled_delay_ms.map(|v| v as i64))
        .bind(event.evidence.as_deref())
        .execute(&mut *tx)
        .await?;

        if event.event_type == IncidentEventType::ReconnectAttemptFailed {
            sqlx::query(
                "UPDATE monitor_incidents
                 SET reconnect_attempts = reconnect_attempts + 1, updated_at = ?
                 WHERE id = ?",
            )
            .bind(rfc3339(Utc::now()))
            .bind(incident_id.as_str())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Record a transport recovery timestamp on an open incident.
    pub async fn record_transport_recovered(
        &self,
        incident_id: &IncidentId,
        recovered_at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE monitor_incidents SET transport_recovered_at = ?, updated_at = ? WHERE id = ?",
        )
        .bind(rfc3339(recovered_at))
        .bind(rfc3339(Utc::now()))
        .bind(incident_id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Resolve an open incident with delivery recovery ending at `resolved_at`.
    /// Recovery measurements distinguish total silence from detected recovery.
    pub async fn resolve_incident(
        &self,
        incident_id: &IncidentId,
        resolved_at: DateTime<Utc>,
    ) -> Result<()> {
        let summary = match self.get_incident(incident_id).await? {
            Some(summary) if summary.state == IncidentState::Open => summary,
            _ => return Ok(()),
        };
        let total_silence_ms = summary
            .last_useful_record_at
            .map(|start| {
                resolved_at
                    .signed_duration_since(start)
                    .num_milliseconds()
                    .max(0)
            })
            .unwrap_or_else(|| {
                resolved_at
                    .signed_duration_since(summary.detected_at)
                    .num_milliseconds()
                    .max(0)
            }) as u64;
        let detected_recovery_ms = resolved_at
            .signed_duration_since(summary.detected_at)
            .num_milliseconds()
            .max(0) as u64;

        let summary = IncidentSummary {
            state: IncidentState::Resolved,
            resolved_at: Some(resolved_at),
            total_silence_ms: Some(total_silence_ms as u64),
            detected_recovery_ms: Some(detected_recovery_ms),
            updated_at: Utc::now(),
            ..summary
        };
        self.update_summary(&summary).await
    }

    /// Update storeable summary fields from a summary struct.
    pub async fn update_summary(&self, summary: &IncidentSummary) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE monitor_incidents SET
                state = ?, trigger = ?, last_useful_record_at = ?, detected_at = ?,
                resolved_at = ?, transport_recovered_at = ?, total_silence_ms = ?,
                detected_recovery_ms = ?, reconnect_attempts = ?, connection_epoch = ?,
                observation_complete = ?, monitor_process_epoch = ?, monitor_release = ?,
                updated_at = ?
            WHERE id = ?
            "#,
        )
        .bind(state_str(summary.state))
        .bind(trigger_str(summary.trigger))
        .bind(summary.last_useful_record_at.map(rfc3339))
        .bind(rfc3339(summary.detected_at))
        .bind(summary.resolved_at.map(rfc3339))
        .bind(summary.transport_recovered_at.map(rfc3339))
        .bind(summary.total_silence_ms.map(|v| v as i64))
        .bind(summary.detected_recovery_ms.map(|v| v as i64))
        .bind(summary.reconnect_attempts as i64)
        .bind(summary.connection_epoch as i64)
        .bind(if summary.observation_complete { 1 } else { 0 })
        .bind(&summary.monitor_process_epoch)
        .bind(&summary.monitor_release)
        .bind(rfc3339(summary.updated_at))
        .bind(summary.id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Terminate all incidents left open by a previous monitor process as
    /// `incomplete` with an observation-gap event, in one transaction.
    pub async fn reconcile_open_incidents(
        &self,
        _identity: &MonitorIdentity,
        now: DateTime<Utc>,
    ) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let open_ids: Vec<(String,)> =
            sqlx::query_as("SELECT id FROM monitor_incidents WHERE state = 'open'")
                .fetch_all(&mut *tx)
                .await?;

        for (id,) in open_ids {
            let max_sequence: i64 = sqlx::query_scalar(
                "SELECT COALESCE(MAX(sequence), 0) FROM monitor_incident_events WHERE incident_id = ?",
            )
            .bind(&id)
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(0);
            sqlx::query(
                r#"
                INSERT INTO monitor_incident_events (
                    incident_id, sequence, event_type, occurred_at, reason
                )
                VALUES (?, ?, 'observation_gap', ?, 'observation_gap')
                "#,
            )
            .bind(&id)
            .bind(max_sequence + 1)
            .bind(rfc3339(now))
            .execute(&mut *tx)
            .await?;
        }

        let result = sqlx::query(
            r#"
            UPDATE monitor_incidents
            SET state = 'incomplete', observation_complete = 0, updated_at = ?
            WHERE state = 'open'
            "#,
        )
        .bind(rfc3339(now))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(result.rows_affected())
    }

    /// Delete terminal-state incidents older than the retention boundary and
    /// all of their events in one cleanup transaction. Open incidents remain.
    pub async fn retention_cleanup(&self, retention_days: u64) -> Result<u64> {
        let cutoff = Utc::now() - Duration::days(retention_days as i64);
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            r#"
            DELETE FROM monitor_incidents
            WHERE state IN ('resolved', 'incomplete')
              AND COALESCE(resolved_at, updated_at, detected_at) < ?
            "#,
        )
        .bind(rfc3339(cutoff))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    /// Number of retained incidents (all states), for metrics/health.
    pub async fn incident_count(&self) -> Result<u64> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM monitor_incidents")
            .fetch_one(&self.pool)
            .await?;
        Ok(count.max(0) as u64)
    }

    /// List incident summaries with keyset pagination ordered by
    /// `(detected_at DESC, id DESC)`.
    pub async fn list_incidents(
        &self,
        filter: &IncidentFilter,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<IncidentPage> {
        let cursor_vals = match cursor {
            Some(c) => Some(decode_incident_cursor(c)?),
            None => None,
        };

        let mut query = sqlx::QueryBuilder::new(
            r#"
            SELECT id, stream_id, state, trigger, last_useful_record_at, detected_at,
                   resolved_at, transport_recovered_at, total_silence_ms,
                   detected_recovery_ms, reconnect_attempts, connection_epoch,
                   observation_complete, monitor_process_epoch, monitor_release,
                   created_at, updated_at
            FROM monitor_incidents
            WHERE 1 = 1
            "#,
        );
        if let Some(stream) = &filter.stream_id {
            query.push(" AND stream_id = ").push_bind(stream.clone());
        }
        if let Some(state) = filter.state {
            query
                .push(" AND state = ")
                .push_bind(state_str(state).to_string());
        }
        if let Some(trigger) = filter.trigger {
            query
                .push(" AND trigger = ")
                .push_bind(trigger_str(trigger).to_string());
        }
        if let Some(from) = filter.detected_from {
            query.push(" AND detected_at >= ").push_bind(rfc3339(from));
        }
        if let Some(to) = filter.detected_to {
            query.push(" AND detected_at <= ").push_bind(rfc3339(to));
        }
        if let Some(min) = filter.min_silence_ms {
            query
                .push(" AND total_silence_ms >= ")
                .push_bind(min as i64);
        }
        if let Some((time, id)) = &cursor_vals {
            query
                .push(" AND (detected_at < ")
                .push_bind(time.clone())
                .push(" OR (detected_at = ")
                .push_bind(time.clone())
                .push(" AND id < ")
                .push_bind(id.clone())
                .push("))");
        }
        query
            .push(" ORDER BY detected_at DESC, id DESC LIMIT ")
            .push_bind(limit as i64 + 1);
        let rows: Vec<SummaryRow> = query.build_query_as().fetch_all(&self.pool).await?;

        let mut incidents = Vec::with_capacity(rows.len());
        for row in rows {
            incidents.push(row_to_summary(row)?);
        }

        let next_cursor = if incidents.len() > limit {
            incidents.truncate(limit);
            let last = incidents.last().expect("page truncated above zero");
            Some(encode_incident_cursor(
                &rfc3339(last.detected_at),
                last.id.as_str(),
            ))
        } else {
            None
        };

        Ok(IncidentPage {
            incidents,
            next_cursor,
        })
    }

    /// Fetch one incident summary by ID.
    pub async fn get_incident(&self, incident_id: &IncidentId) -> Result<Option<IncidentSummary>> {
        let row: Option<SummaryRow> = sqlx::query_as(
            r#"
            SELECT id, stream_id, state, trigger, last_useful_record_at, detected_at,
                   resolved_at, transport_recovered_at, total_silence_ms,
                   detected_recovery_ms, reconnect_attempts, connection_epoch,
                   observation_complete, monitor_process_epoch, monitor_release,
                   created_at, updated_at
            FROM monitor_incidents
            WHERE id = ?
            "#,
        )
        .bind(incident_id.as_str())
        .fetch_optional(&self.pool)
        .await?;

        Ok(match row {
            Some(row) => Some(row_to_summary(row)?),
            None => None,
        })
    }

    /// Paginate events in ascending incident-local sequence order.
    pub async fn list_incident_events(
        &self,
        incident_id: &IncidentId,
        limit: usize,
        after_sequence: Option<i64>,
    ) -> Result<EventPage> {
        let mut query = sqlx::QueryBuilder::new(
            r#"
                SELECT sequence, event_type, occurred_at, reason, attempt_ordinal, scheduled_delay_ms, evidence
                FROM monitor_incident_events
                WHERE incident_id = "#,
        );
        query.push_bind(incident_id.as_str());
        if let Some(after) = after_sequence {
            query.push(" AND sequence > ").push_bind(after);
        }
        query
            .push(" ORDER BY sequence ASC LIMIT ")
            .push_bind(limit as i64 + 1);
        let rows: Vec<EventRow> = query.build_query_as().fetch_all(&self.pool).await?;

        let mut events = Vec::with_capacity(rows.len());
        for (
            sequence,
            event_type_str,
            occurred_at,
            reason,
            attempt_ordinal,
            scheduled_delay_ms,
            evidence,
        ) in rows
        {
            let event_type: IncidentEventType =
                serde_json::from_str(&format!("\"{event_type_str}\""))
                    .map_err(|e| anyhow::anyhow!("unknown event type {event_type_str}: {e}"))?;
            events.push(IncidentEvent {
                evidence,
                sequence,
                event_type,
                occurred_at: parse_time(Some(occurred_at))?
                    .ok_or_else(|| anyhow::anyhow!("missing event occurred_at"))?,
                reason,
                attempt_ordinal: attempt_ordinal.map(|v: i64| v.max(0) as u64),
                scheduled_delay_ms: scheduled_delay_ms.map(|v: i64| v.max(0) as u64),
            });
        }

        let next_cursor = if events.len() > limit {
            events.truncate(limit);
            events.last().map(|e| e.sequence)
        } else {
            None
        };

        Ok(EventPage {
            events,
            next_cursor,
        })
    }

    /// Highest event sequence currently stored for an incident.
    pub async fn max_event_sequence(&self, incident_id: &IncidentId) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT COALESCE(MAX(sequence), 0) FROM monitor_incident_events WHERE incident_id = ?",
        )
        .bind(incident_id.as_str())
        .fetch_one(&self.pool)
        .await
        .unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::incidents::{HandshakeFailureReason, MonitorIdentity};
    use std::str::FromStr;

    async fn new_store(test_name: &str) -> (IncidentStore, SqlitePool) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("incident-ledger-{test_name}-{nanos}.db"));
        let options = sqlx::sqlite::SqliteConnectOptions::from_str(&format!(
            "sqlite://{}?mode=rwc",
            path.display()
        ))
        .unwrap()
        .foreign_keys(true)
        .create_if_missing(true);
        let pool = SqlitePool::connect_with(options).await.unwrap();
        let store = IncidentStore::new(pool.clone()).await.unwrap();
        (store, pool)
    }

    fn identity() -> MonitorIdentity {
        MonitorIdentity {
            process_epoch: "test-epoch".to_string(),
            release: "0.1.0".to_string(),
        }
    }

    async fn open_test_incident(store: &IncidentStore, stream: &str, id: &str) -> IncidentId {
        let incident_id = IncidentId::from_string(id).expect("valid test id");
        store
            .open_incident(
                &incident_id,
                stream,
                IncidentTrigger::DeliveryIdle,
                Utc::now(),
                Some(Utc::now() - Duration::seconds(30)),
                1,
                &identity(),
            )
            .await
            .unwrap();
        incident_id
    }

    async fn attempt_event(store: &IncidentStore, incident: &IncidentId, sequence: i64) {
        store
            .append_event(
                incident,
                IncidentEvent {
                    sequence,
                    event_type: IncidentEventType::ReconnectAttemptFailed,
                    occurred_at: Utc::now(),
                    reason: Some(HandshakeFailureReason::ConnectTimeout.as_str().to_string()),
                    attempt_ordinal: Some(sequence as u64),
                    scheduled_delay_ms: Some(1_000),
                    evidence: None,
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn attempt_events_update_summary_atomically() -> Result<()> {
        let (store, _pool) = new_store("attempt-summary").await;
        let incident = open_test_incident(&store, "a", "01ARZ3NDEKTSV4RRFFQ69G5FAZ").await;

        attempt_event(&store, &incident, 1).await;
        attempt_event(&store, &incident, 2).await;

        let summary = store.get_incident(&incident).await?.expect("exists");
        assert_eq!(summary.reconnect_attempts, 2);
        assert_eq!(summary.state, IncidentState::Open);
        assert_eq!(store.max_event_sequence(&incident).await?, 2);
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_sequence_is_rejected() -> Result<()> {
        let (store, _pool) = new_store("duplicate-sequence").await;
        let incident = open_test_incident(&store, "a", "01ARZ3NDEKTSV4RRFFQ69G5FBX").await;

        attempt_event(&store, &incident, 1).await;
        let duplicate = store
            .append_event(
                &incident,
                IncidentEvent {
                    evidence: None,
                    sequence: 1,
                    event_type: IncidentEventType::ReconnectAttemptFailed,
                    occurred_at: Utc::now(),
                    reason: Some("connect_error".to_string()),
                    attempt_ordinal: Some(1),
                    scheduled_delay_ms: None,
                },
            )
            .await;

        assert!(
            duplicate.is_err(),
            "unique (incident_id, sequence) must reject"
        );
        // The failed insert must not have incremented the summary.
        let summary = store.get_incident(&incident).await?.expect("exists");
        assert_eq!(summary.reconnect_attempts, 1);
        Ok(())
    }

    #[tokio::test]
    async fn restart_reconciliation_marks_incomplete_with_observation_gap() -> Result<()> {
        let (store, _pool) = new_store("restart-reconcile").await;
        let incident = open_test_incident(&store, "b", "01ARZ3NDEKTSV4RRFFQ69G5FCX").await;

        let reconciled = store
            .reconcile_open_incidents(&identity(), Utc::now())
            .await?;
        assert_eq!(reconciled, 1);

        let summary = store.get_incident(&incident).await?.expect("exists");
        assert_eq!(summary.state, IncidentState::Incomplete);
        assert!(!summary.observation_complete);
        assert!(summary.resolved_at.is_none());

        let events = store.list_incident_events(&incident, 10, None).await?;
        assert_eq!(events.events.len(), 1);
        assert_eq!(
            events.events[0].event_type,
            IncidentEventType::ObservationGap
        );

        // Running reconciliation again must not find anything open.
        assert_eq!(
            store
                .reconcile_open_incidents(&identity(), Utc::now())
                .await?,
            0
        );
        Ok(())
    }

    #[tokio::test]
    async fn retention_removes_expired_terminal_incidents_transactionally() -> Result<()> {
        let (store, _pool) = new_store("retention-cascade").await;
        let old_incident = open_test_incident(&store, "a", "01ARZ3NDEKTSV4RRFFQ69G5FDX").await;
        attempt_event(&store, &old_incident, 1).await;

        // Resolve it so it becomes terminal, then age it out artificially.
        store
            .resolve_incident(&old_incident, Utc::now() - Duration::days(100))
            .await?;
        // Backdate updated_at/resolved_at directly through the summary update.
        let mut summary = store.get_incident(&old_incident).await?.unwrap();
        summary.updated_at = Utc::now() - Duration::days(100);
        summary.resolved_at = Some(Utc::now() - Duration::days(100));
        store.update_summary(&summary).await?;

        // Open incidents are retained even when their data ages past the boundary.
        let _open_incident = open_test_incident(&store, "b", "01ARZ3NDEKTSV4RRFFQ69G5FEX").await;

        let removed = store.retention_cleanup(90).await?;
        assert_eq!(removed, 1, "only the expired terminal incident is removed");

        assert!(store.get_incident(&old_incident).await?.is_none());
        assert!(store
            .list_incident_events(&old_incident, 10, None)
            .await?
            .events
            .is_empty());
        let open_summary = store
            .get_incident(&_open_incident)
            .await?
            .expect("open kept");
        assert_eq!(open_summary.state, IncidentState::Open);
        Ok(())
    }

    #[tokio::test]
    async fn keyset_pagination_walks_without_gaps_or_duplicates() -> anyhow::Result<()> {
        let (store, _pool) = new_store("keyset-paging").await;
        let mut ids = Vec::new();
        for (index, id) in [
            "01ARZ3NDEKTSV4RRFFQ69G5FFX",
            "01ARZ3NDEKTSV4RRFFQ69G5FGX",
            "01ARZ3NDEKTSV4RRFFQ69G5FHX",
            "01ARZ3NDEKTSV4RRFFQ69G5FIX",
            "01ARZ3NDEKTSV4RRFFQ69G5FJX",
            "01ARZ3NDEKTSV4RRFFQ69G5FKX",
        ]
        .into_iter()
        .enumerate()
        {
            let incident = open_test_incident(&store, "a", id).await;
            // Stagger detected_at by creation order through updates.
            let mut summary = store.get_incident(&incident).await.unwrap().unwrap();
            summary.detected_at = Utc::now() - chrono::Duration::minutes(10)
                + chrono::Duration::seconds(index as i64);
            summary.updated_at = summary.detected_at;
            store.update_summary(&summary).await.unwrap();
            ids.push(incident);
        }

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = store
                .list_incidents(&IncidentFilter::default(), 2, cursor.as_deref())
                .await?;
            assert!(page.incidents.len() <= 2);
            for incident in &page.incidents {
                seen.push(incident.id.clone());
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(seen.len(), 6, "all incidents paged exactly once");
        assert_eq!(
            seen.iter().collect::<std::collections::HashSet<_>>().len(),
            seen.len()
        );

        // Newest-first keyset ordering.
        let ordered: Vec<String> = store
            .list_incidents(&IncidentFilter::default(), 10, None)
            .await?
            .incidents
            .into_iter()
            .map(|i| i.id.as_str().to_string())
            .collect();
        let expected_newest_first: Vec<String> = ids
            .iter()
            .rev()
            .map(|id: &IncidentId| id.as_str().to_string())
            .collect();
        assert_eq!(ordered, expected_newest_first);
        Ok(())
    }

    #[tokio::test]
    async fn filters_apply_before_cursor_pages() -> anyhow::Result<()> {
        use crate::incidents::IncidentState;

        let (store, _pool) = new_store("filters").await;
        let a = open_test_incident(&store, "a", "01ARZ3NDEKTSV4RRFFQ69G5FMX").await;
        let b = open_test_incident(&store, "b", "01ARZ3NDEKTSV4RRFFQ69G5FNX").await;
        store.resolve_incident(&b, Utc::now()).await?;

        let stream_filter = IncidentFilter {
            stream_id: Some("a".to_string()),
            ..Default::default()
        };
        let page = store.list_incidents(&stream_filter, 50, None).await?;
        assert_eq!(page.incidents.len(), 1);
        assert_eq!(page.incidents[0].id, a);

        let state_filter = IncidentFilter {
            state: Some(IncidentState::Resolved),
            ..Default::default()
        };
        let page = store.list_incidents(&state_filter, 50, None).await?;
        assert_eq!(page.incidents.len(), 1);
        assert_eq!(page.incidents[0].id, b);

        let trigger_filter = IncidentFilter {
            trigger: Some(IncidentTrigger::TransportLoss),
            ..Default::default()
        };
        assert!(store
            .list_incidents(&trigger_filter, 50, None)
            .await?
            .incidents
            .is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn unknown_incident_returns_none() -> anyhow::Result<()> {
        let (store, _pool) = new_store("unknown-incident").await;
        let unknown = IncidentId::from_string("01ARZ3NDEKTSV4RRFFQ69G5FOX").unwrap();
        assert!(store.get_incident(&unknown).await?.is_none());
        Ok(())
    }
}
