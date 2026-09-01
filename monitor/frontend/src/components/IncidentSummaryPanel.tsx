import { useEffect, useState } from "react"
import { ExternalLink, ShieldAlert } from "lucide-react"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import {
  activeIncidents,
  buildIncidentsUrl,
  incidentDetailUrl,
  parseIncidentList,
  type IncidentSummary,
} from "@/lib/incidents"

interface IncidentSummaryPanelProps {
  /** Base URL for the monitor API; defaults to same-origin. */
  baseUrl?: string
  /** Poll interval in milliseconds. */
  refreshMs?: number
}

function formatState(state: IncidentSummary["state"]): string {
  switch (state) {
    case "open":
      return "Open"
    case "resolved":
      return "Resolved"
    case "incomplete":
      return "Incomplete (observation gap)"
  }
}

function formatTrigger(trigger: IncidentSummary["trigger"]): string {
  switch (trigger) {
    case "delivery_idle":
      return "delivery idle"
    case "transport_loss":
      return "transport loss"
    case "duplicate_delivery":
      return "duplicate delivery"
    case "ordinal_gap":
      return "ordinal gap"
    default:
      return trigger
  }
}

export function IncidentSummaryPanel({
  baseUrl = "",
  refreshMs = 15000,
}: IncidentSummaryPanelProps) {
  const [incidents, setIncidents] = useState<IncidentSummary[]>([])
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    const controller = new AbortController()

    const load = async () => {
      try {
        const res = await fetch(buildIncidentsUrl(baseUrl), {
          signal: controller.signal,
          headers: { accept: "application/json" },
        })
        if (!res.ok) throw new Error(`incidents API returned ${res.status}`)
        const json = await res.json()
        if (cancelled) return
        setIncidents(parseIncidentList(json))
        setError(null)
      } catch (e) {
        if (cancelled || (e instanceof DOMException && e.name === "AbortError")) return
        setError(e instanceof Error ? e.message : "Unknown error")
      }
    }

    load()
    const interval = window.setInterval(load, refreshMs)
    return () => {
      cancelled = true
      clearInterval(interval)
      controller.abort()
    }
  }, [baseUrl, refreshMs])

  const open = activeIncidents(incidents)

  return (
    <Card className="monitor-panel" aria-labelledby="incident-summary-title">
      <CardHeader className="monitor-table-card-header">
        <CardTitle className="monitor-chart-title">
          DELIVERY INCIDENTS
          <span className="ml-2 monitor-table-head">
            <ShieldAlert aria-hidden="true" />
          </span>
        </CardTitle>
      </CardHeader>
      <CardContent className="monitor-table-card-content">
        {error !== null ? (
          <p className="monitor-table-state">Incident ledger unavailable: {error}</p>
        ) : incidents.length === 0 ? (
          <p className="monitor-table-state">No incidents retained.</p>
        ) : open.length > 0 ? (
          <p className="monitor-recovery-summary-value">
            {open.length} active incident{open.length === 1 ? "" : "s"}
          </p>
        ) : (
          <p className="monitor-recovery-summary-value">No active incidents</p>
        )}
        <ul className="monitor-incident-list">
          {incidents.slice(0, 10).map((incident) => (
            <li key={incident.id} className="monitor-incident-row">
              <a
                className="monitor-incident-link"
                href={incidentDetailUrl(baseUrl, incident.id)}
                target="_blank"
                rel="noreferrer"
              >
                <span
                  className={`monitor-incident-state monitor-incident-state--${incident.state}`}
                >
                  {formatState(incident.state)}
                </span>
                <span className="monitor-incident-detail">
                  {incident.stream_id} · {formatTrigger(incident.trigger)} ·{" "}
                  {incident.reconnect_attempts} reconnect attempt
                  {incident.reconnect_attempts === 1 ? "" : "s"} ·
                  started {new Date(incident.detected_at).toLocaleString()}
                </span>
                <span className="monitor-incident-open-hint">
                  JSON detail
                  <ExternalLink className="inline h-3 w-3" aria-hidden="true" />
                </span>
              </a>
            </li>
          ))}
        </ul>
      </CardContent>
    </Card>
  )
}
