/**
 * Typed client helpers for the versioned incident API (/api/v1/incidents).
 */

export type IncidentState = "open" | "resolved" | "incomplete"
export type IncidentTrigger = "delivery_idle" | "transport_loss"

export interface IncidentSummary {
  id: string
  stream_id: string
  state: IncidentState
  trigger: IncidentTrigger
  detected_at: string
  resolved_at: string | null
  transport_recovered_at: string | null
  total_silence_ms: number | null
  detected_recovery_ms: number | null
  reconnect_attempts: number
  connection_epoch: number
  observation_complete: boolean
  monitor_release: string
}

export interface IncidentListResponse {
  data: {
    incidents: IncidentSummary[]
    next_cursor: string | null
  }
}

export function buildIncidentsUrl(baseUrl: string, limit = 10): string {
  const cleaned = baseUrl.replace(/\/+$/, "")
  return `${cleaned}/api/v1/incidents?limit=${limit}`
}

export function incidentDetailUrl(baseUrl: string, incidentId: string): string {
  const cleaned = baseUrl.replace(/\/+$/, "")
  return `${cleaned}/api/v1/incidents/${encodeURIComponent(incidentId)}`
}

export function parseIncidentList(response: unknown): IncidentSummary[] {
  if (response === null || typeof response !== "object") return []
  const data = (response as { data?: { incidents?: unknown } }).data
  const incidents = data?.incidents
  if (!Array.isArray(incidents)) return []
  return incidents.filter(
    (incident): incident is IncidentSummary =>
      incident !== null &&
      typeof incident === "object" &&
      typeof (incident as IncidentSummary).id === "string" &&
      typeof (incident as IncidentSummary).state === "string",
  )
}

export function activeIncidents(incidents: IncidentSummary[]): IncidentSummary[] {
  return incidents.filter((incident) => incident.state === "open")
}
