//! `/api/v1/incidents` handlers: sanitized DTOs, query validation, and
//! cursor pagination over the durable incident ledger.

use axum::http::{header, StatusCode};
use axum::response::Response;
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{ApiError, ApiResponse, NO_STORE};
use crate::incidents::store::{IncidentFilter, IncidentPage};
use crate::incidents::{IncidentId, IncidentState, IncidentSummary, IncidentTrigger};

/// Default page size for incident lists.
pub const INCIDENT_LIST_DEFAULT_LIMIT: usize = 50;
/// Maximum page size for incident lists.
pub const INCIDENT_LIST_MAX_LIMIT: usize = 200;
/// Default page size for incident event lists.
pub const INCIDENT_EVENTS_DEFAULT_LIMIT: usize = 100;
/// Maximum page size for incident event lists.
pub const INCIDENT_EVENTS_MAX_LIMIT: usize = 500;

/// Validated incident-list query parameters.
#[derive(Debug, Clone, Default)]
pub struct IncidentListQuery {
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    pub stream: Option<String>,
    pub state: Option<IncidentState>,
    pub trigger: Option<IncidentTrigger>,
    pub detected_from: Option<DateTime<Utc>>,
    pub detected_to: Option<DateTime<Utc>>,
    pub min_silence_ms: Option<u64>,
}

pub fn parse_limit(raw: Option<&str>, default: usize, max: usize) -> Result<usize, ApiError> {
    match raw {
        None => Ok(default),
        Some(value) => value
            .parse::<usize>()
            .map_err(|_| ApiError::invalid_request("limit must be a positive integer"))
            .and_then(|v| {
                if v == 0 {
                    Err(ApiError::invalid_request("limit must be greater than zero"))
                } else if v > max {
                    Err(ApiError::invalid_request(format!(
                        "limit must not exceed {max}"
                    )))
                } else {
                    Ok(v)
                }
            }),
    }
}

pub fn parse_stream(raw: Option<&str>) -> Result<Option<String>, ApiError> {
    match raw {
        None => Ok(None),
        Some(stream) => {
            if matches!(stream, "a" | "b" | "baseline1" | "baseline2") {
                Ok(Some(stream.to_string()))
            } else {
                Err(ApiError::invalid_request(format!(
                    "unsupported stream value {stream}"
                )))
            }
        }
    }
}

pub fn parse_state(raw: Option<&str>) -> Result<Option<IncidentState>, ApiError> {
    match raw {
        None => Ok(None),
        Some("open") => Ok(Some(IncidentState::Open)),
        Some("resolved") => Ok(Some(IncidentState::Resolved)),
        Some("incomplete") => Ok(Some(IncidentState::Incomplete)),
        Some(other) => Err(ApiError::invalid_request(format!(
            "unsupported state value {other}"
        ))),
    }
}

pub fn parse_trigger(raw: Option<&str>) -> Result<Option<IncidentTrigger>, ApiError> {
    match raw {
        None => Ok(None),
        Some("delivery_idle") => Ok(Some(IncidentTrigger::DeliveryIdle)),
        Some("transport_loss") => Ok(Some(IncidentTrigger::TransportLoss)),
        Some("duplicate_delivery") => Ok(Some(IncidentTrigger::DuplicateDelivery)),
        Some("ordinal_gap") => Ok(Some(IncidentTrigger::OrdinalGap)),
        Some(other) => Err(ApiError::invalid_request(format!(
            "unsupported trigger value {other}"
        ))),
    }
}

fn parse_time_raw(raw: Option<&str>, label: &str) -> Result<Option<DateTime<Utc>>, ApiError> {
    match raw {
        None => Ok(None),
        Some(value) => DateTime::parse_from_rfc3339(value)
            .map(|dt| dt.with_timezone(&Utc))
            .map(Some)
            .map_err(|_| {
                ApiError::invalid_request(format!("{label} must be an RFC 3339 timestamp"))
            }),
    }
}

pub fn build_filter(
    stream: Option<&str>,
    state: Option<&str>,
    trigger: Option<&str>,
    detected_from: Option<&str>,
    detected_to: Option<&str>,
    min_silence_ms: Option<&str>,
) -> Result<IncidentFilter, ApiError> {
    let min_silence_ms = match min_silence_ms {
        None => None,
        Some(value) => value.parse::<u64>().map(Some).map_err(|_| {
            ApiError::invalid_request("min_silence_ms must be a non-negative integer")
        })?,
    };
    Ok(IncidentFilter {
        stream_id: parse_stream(stream)?,
        state: parse_state(state)?,
        trigger: parse_trigger(trigger)?,
        detected_from: parse_time_raw(detected_from, "detected_from")?,
        detected_to: parse_time_raw(detected_to, "detected_to")?,
        min_silence_ms,
    })
}

fn response(status: StatusCode, body: impl Serialize) -> Response {
    let body = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, NO_STORE)
        .body(axum::body::Body::from(body))
        .unwrap()
}

fn api_error_response(error: &ApiError) -> Response {
    let status = match error.code {
        super::ApiErrorCode::InvalidRequest => StatusCode::BAD_REQUEST,
        super::ApiErrorCode::NotFound => StatusCode::NOT_FOUND,
        super::ApiErrorCode::StorageUnavailable | super::ApiErrorCode::InternalError => {
            StatusCode::SERVICE_UNAVAILABLE
        }
    };
    response(status, error)
}

/// Serializable incident summary with RFC 3339 timestamps and nullable
/// recovery fields.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct IncidentSummaryDto {
    pub id: String,
    pub stream_id: String,
    pub state: String,
    pub trigger: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_useful_record_at: Option<String>,
    pub detected_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport_recovered_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_silence_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_recovery_ms: Option<u64>,
    pub reconnect_attempts: u64,
    pub connection_epoch: u64,
    pub observation_complete: bool,
    pub monitor_process_epoch: String,
    pub monitor_release: String,
    pub created_at: String,
    pub updated_at: String,
}

impl From<&IncidentSummary> for IncidentSummaryDto {
    fn from(summary: &IncidentSummary) -> Self {
        let state = match summary.state {
            crate::incidents::IncidentState::Open => "open",
            crate::incidents::IncidentState::Resolved => "resolved",
            crate::incidents::IncidentState::Incomplete => "incomplete",
        };
        let trigger = match summary.trigger {
            IncidentTrigger::DeliveryIdle => "delivery_idle",
            IncidentTrigger::TransportLoss => "transport_loss",
            IncidentTrigger::DuplicateDelivery => "duplicate_delivery",
            IncidentTrigger::OrdinalGap => "ordinal_gap",
        };
        Self {
            id: summary.id.as_str().to_string(),
            stream_id: summary.stream_id.clone(),
            state: state.to_string(),
            trigger: trigger.to_string(),
            last_useful_record_at: summary
                .last_useful_record_at
                .map(|v| v.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            detected_at: summary
                .detected_at
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            resolved_at: summary
                .resolved_at
                .map(|v| v.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            transport_recovered_at: summary
                .transport_recovered_at
                .map(|v| v.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            total_silence_ms: summary.total_silence_ms,
            detected_recovery_ms: summary.detected_recovery_ms,
            reconnect_attempts: summary.reconnect_attempts,
            connection_epoch: summary.connection_epoch,
            observation_complete: summary.observation_complete,
            monitor_process_epoch: summary.monitor_process_epoch.clone(),
            monitor_release: summary.monitor_release.clone(),
            created_at: summary
                .created_at
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            updated_at: summary
                .updated_at
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        }
    }
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct IncidentListDto {
    pub incidents: Vec<IncidentSummaryDto>,
    pub next_cursor: Option<String>,
}

/// Core list implementation shared by the HTTP handler and tests.
pub async fn list_incidents_response(
    store: &crate::incidents::IncidentStore,
    query: &IncidentListQuery,
) -> Response {
    if query
        .limit
        .is_some_and(|limit| limit > INCIDENT_LIST_MAX_LIMIT)
    {
        return api_error_response(&ApiError::invalid_request(format!(
            "limit must not exceed {INCIDENT_LIST_MAX_LIMIT}"
        )));
    }
    if let Some(cursor) = &query.cursor {
        if let Err(reason) = crate::incidents::store::validate_incident_cursor(cursor) {
            return api_error_response(&ApiError::invalid_request(reason));
        }
    }
    let limit = query.limit.unwrap_or(INCIDENT_LIST_DEFAULT_LIMIT);
    let filter = IncidentFilter {
        stream_id: query.stream.clone(),
        state: query.state,
        trigger: query.trigger,
        detected_from: query.detected_from,
        detected_to: query.detected_to,
        min_silence_ms: query.min_silence_ms,
    };
    match store
        .list_incidents(&filter, limit, query.cursor.as_deref())
        .await
    {
        Ok(IncidentPage {
            incidents,
            next_cursor,
        }) => {
            let dtos: Vec<IncidentSummaryDto> =
                incidents.iter().map(IncidentSummaryDto::from).collect();
            response(
                StatusCode::OK,
                ApiResponse {
                    data: IncidentListDto {
                        incidents: dtos,
                        next_cursor,
                    },
                },
            )
        }
        Err(_) => api_error_response(&ApiError::storage_unavailable()),
    }
}

pub async fn incident_detail_response(
    store: &crate::incidents::IncidentStore,
    id: &str,
) -> Response {
    let Some(incident_id) = IncidentId::from_string(id.to_string()) else {
        return api_error_response(&ApiError::not_found());
    };
    match store.get_incident(&incident_id).await {
        Ok(Some(summary)) => response(
            StatusCode::OK,
            ApiResponse {
                data: IncidentSummaryDto::from(&summary),
            },
        ),
        Ok(None) => api_error_response(&ApiError::not_found()),
        Err(_) => api_error_response(&ApiError::storage_unavailable()),
    }
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct IncidentEventDto {
    pub sequence: i64,
    pub event_type: String,
    pub occurred_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_ordinal: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduled_delay_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct IncidentEventListDto {
    pub incident_id: String,
    pub events: Vec<IncidentEventDto>,
    pub next_cursor: Option<i64>,
}

pub async fn incident_events_response(
    store: &crate::incidents::IncidentStore,
    id: &str,
    limit_raw: Option<&str>,
    cursor_raw: Option<&str>,
) -> Response {
    let Some(incident_id) = IncidentId::from_string(id.to_string()) else {
        return api_error_response(&ApiError::not_found());
    };
    let limit = match parse_event_limit(limit_raw) {
        Ok(limit) => limit,
        Err(error) => return api_error_response(&error),
    };
    let after_sequence = match cursor_raw {
        None => None,
        Some(raw) => match raw.parse::<i64>() {
            Ok(value) if value >= 0 => Some(value),
            _ => {
                return api_error_response(&ApiError::invalid_request(
                    "invalid events cursor; expected a non-negative sequence",
                ))
            }
        },
    };
    // Verify the incident exists so unknown IDs are 404, not empty pages.
    match store.get_incident(&incident_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return api_error_response(&ApiError::not_found()),
        Err(_) => return api_error_response(&ApiError::storage_unavailable()),
    }
    match store
        .list_incident_events(&incident_id, limit, after_sequence)
        .await
    {
        Ok(page) => {
            let events: Vec<IncidentEventDto> = page
                .events
                .iter()
                .map(|event| IncidentEventDto {
                    sequence: event.sequence,
                    event_type: serde_json::to_string(&event.event_type)
                        .unwrap_or_default()
                        .trim_matches('"')
                        .to_string(),
                    occurred_at: event
                        .occurred_at
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    reason: event.reason.clone(),
                    attempt_ordinal: event.attempt_ordinal,
                    scheduled_delay_ms: event.scheduled_delay_ms,
                })
                .collect();
            response(
                StatusCode::OK,
                ApiResponse {
                    data: IncidentEventListDto {
                        incident_id: incident_id.as_str().to_string(),
                        events,
                        next_cursor: page.next_cursor,
                    },
                },
            )
        }
        Err(_) => api_error_response(&ApiError::storage_unavailable()),
    }
}

pub fn parse_event_limit(raw: Option<&str>) -> Result<usize, ApiError> {
    parse_limit(
        raw,
        INCIDENT_EVENTS_DEFAULT_LIMIT,
        INCIDENT_EVENTS_MAX_LIMIT,
    )
}

/// Validated query parameters for the incident list endpoint.
#[derive(Debug, Clone, Default, serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct IncidentQueryParams {
    /// Opaque keyset cursor from a previous page.
    #[param(required = false, nullable = true)]
    pub cursor: Option<String>,
    /// Page size, default 50, maximum 200.
    pub limit: Option<usize>,
    /// Stable stream identifier filter (a, b, baseline1, baseline2).
    pub stream: Option<String>,
    /// Incident state filter.
    pub state: Option<String>,
    /// Initial trigger filter.
    pub trigger: Option<String>,
    /// RFC 3339 timestamp: incidents detected at or after this time.
    pub detected_from: Option<String>,
    /// RFC 3339 timestamp: incidents detected at or before this time.
    pub detected_to: Option<String>,
    /// Minimum total silence duration in milliseconds.
    pub min_silence_ms: Option<u64>,
}

impl IncidentQueryParams {
    /// Validate parameters and build the stored filter/query.
    pub fn validated(&self) -> Result<crate::api::incidents::IncidentListQuery, ApiError> {
        Ok(IncidentListQuery {
            cursor: self.cursor.clone(),
            limit: self.limit,
            stream: parse_stream(self.stream.as_deref())?,
            state: parse_state(self.state.as_deref())?,
            trigger: parse_trigger(self.trigger.as_deref())?,
            detected_from: parse_time_raw(self.detected_from.as_deref(), "detected_from")?,
            detected_to: parse_time_raw(self.detected_to.as_deref(), "detected_to")?,
            min_silence_ms: self.min_silence_ms,
        })
    }
}

/// Handler that turns validated parameters into a list response.
pub async fn incidents_response_from_query(
    store: &crate::incidents::IncidentStore,
    params: &IncidentQueryParams,
) -> Response {
    match params.validated() {
        Ok(query) => list_incidents_response(store, &query).await,
        Err(error) => api_error_response(&error),
    }
}

use axum::extract::{Path, Query, State};

/// List incidents with keyset pagination and bounded filters.
#[utoipa::path(
    get,
    path = "/api/v1/incidents",
    operation_id = "listIncidents",
    tag = "incidents",
    params(IncidentQueryParams),
    responses(
        (status = 200, description = "Incident summaries ordered newest first",
         body = inline(ApiResponse<IncidentListDto>),
         headers(("cache-control" = String, description = "no-store"))),
        (status = 400, description = "Invalid cursor, limit, or filter", body = ApiError),
        (status = 503, description = "Incident storage unavailable", body = ApiError),
    )
)]
pub async fn incident_list(
    State(store): State<std::sync::Arc<crate::incidents::IncidentStore>>,
    Query(params): Query<IncidentQueryParams>,
) -> Response {
    incidents_response_from_query(&store, &params).await
}

/// Fetch one incident's sanitized bounded detail.
#[utoipa::path(
    get,
    path = "/api/v1/incidents/{incidentId}",
    operation_id = "getIncident",
    tag = "incidents",
    params(("incidentId" = String, Path, description = "Sortable opaque incident identifier")),
    responses(
        (status = 200, description = "Incident detail",
         body = inline(ApiResponse<IncidentSummaryDto>),
         headers(("cache-control" = String, description = "no-store"))),
        (status = 404, description = "Unknown or expired incident", body = ApiError),
        (status = 503, description = "Incident storage unavailable", body = ApiError),
    )
)]
pub async fn incident_detail(
    State(store): State<std::sync::Arc<crate::incidents::IncidentStore>>,
    Path(incident_id): Path<String>,
) -> Response {
    incident_detail_response(&store, &incident_id).await
}

/// Query parameters for incident event pages.
#[derive(Debug, Clone, Default, serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct IncidentEventsQuery {
    /// Opaque cursor: event sequence after the previous page.
    pub cursor: Option<i64>,
    /// Page size, default 100, maximum 500.
    pub limit: Option<usize>,
}

/// Paginate one incident's ordered events in ascending sequence order.
#[utoipa::path(
    get,
    path = "/api/v1/incidents/{incidentId}/events",
    operation_id = "listIncidentEvents",
    tag = "incidents",
    params(("incidentId" = String, Path, description = "Sortable opaque incident identifier"),
           IncidentEventsQuery),
    responses(
        (status = 200, description = "Ordered incident events",
         body = inline(ApiResponse<IncidentEventListDto>),
         headers(("cache-control" = String, description = "no-store"))),
        (status = 400, description = "Invalid cursor or limit", body = ApiError),
        (status = 404, description = "Unknown or expired incident", body = ApiError),
        (status = 503, description = "Incident storage unavailable", body = ApiError),
    )
)]
pub async fn incident_events(
    State(store): State<std::sync::Arc<crate::incidents::IncidentStore>>,
    Path(incident_id): Path<String>,
    Query(params): Query<IncidentEventsQuery>,
) -> Response {
    incident_events_response(
        &store,
        &incident_id,
        params.limit.map(|v| v.to_string()).as_deref(),
        params.cursor.map(|v| v.to_string()).as_deref(),
    )
    .await
}
