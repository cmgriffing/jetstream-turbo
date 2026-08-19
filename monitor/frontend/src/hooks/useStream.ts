import { useState, useEffect, useRef, useCallback } from 'react'

export type ConnectionStatus = 'connecting' | 'connected' | 'disconnected'
export type DeliveryMode = 'live' | 'catching_up' | 'unknown'
export type ComparisonReason =
  | 'catching_up'
  | 'unknown_mode'
  | 'missing_event_time_coverage'
  | 'watermark_skew'
  | 'disconnected'
  | 'idle_delivery'
  | 'missing_shared_coverage'
  | 'incomplete_identity_coverage'
  | 'settlement_pending'
  | 'legacy_unknown'

export interface StreamEventTime {
  source_watermark_us: number | null
  source_lag_us: number | null
  delivery_mode: DeliveryMode
  event_time_coverage: boolean
  clock_skew_us: number
}

export interface ComparisonEligibility {
  eligible: boolean
  reason: ComparisonReason | null
  watermark_skew_us: number | null
}

export interface PairwiseComparison {
  epoch_id: number | null
  window_start_us: number | null
  window_end_us: number | null
  covered_seconds: number
  left_unique_count: number
  right_unique_count: number
  left_rate: number | null
  right_rate: number | null
  count_delta: number | null
  rate_delta: number | null
  eligible: boolean
  reason: ComparisonReason | null
}

export interface PairwiseComparisons {
  primary: PairwiseComparison
  stream_a_baseline_1: PairwiseComparison
  stream_a_baseline_2: PairwiseComparison
  stream_b_baseline_1: PairwiseComparison
  stream_b_baseline_2: PairwiseComparison
}

export interface StreamStats {
  stream_a?: number
  stream_b?: number
  counting_started_at?: string
  timestamp?: string
  rate_a?: number
  rate_b?: number
  current_streak_a?: number
  current_streak_b?: number
  uptime_a?: number
  uptime_b?: number
  uptime_a_all_time?: number
  uptime_b_all_time?: number
  downtime_a?: number
  downtime_b?: number
  connected_a?: boolean
  connected_b?: boolean
  stream_a_name?: string
  stream_b_name?: string
  baseline_1_name?: string
  baseline_2_name?: string
  baseline_1?: number
  baseline_2?: number
  rate_baseline_1?: number
  rate_baseline_2?: number
  connected_baseline_1?: boolean
  connected_baseline_2?: boolean
  uptime_baseline_1_all_time?: number
  uptime_baseline_2_all_time?: number
  current_streak_baseline_1?: number
  current_streak_baseline_2?: number
  delivery_available_a?: boolean
  delivery_available_b?: boolean
  delivery_available_baseline_1?: boolean
  delivery_available_baseline_2?: boolean
  transport_uptime_a_all_time?: number
  transport_uptime_b_all_time?: number
  transport_uptime_baseline_1_all_time?: number
  transport_uptime_baseline_2_all_time?: number
  delivery_uptime_a_all_time?: number
  delivery_uptime_b_all_time?: number
  delivery_uptime_baseline_1_all_time?: number
  delivery_uptime_baseline_2_all_time?: number
  reconnect_reason_a?: string
  reconnect_reason_b?: string
  reconnect_reason_baseline_1?: string
  reconnect_reason_baseline_2?: string
  data_idle_reconnects_a?: number
  data_idle_reconnects_b?: number
  data_idle_reconnects_baseline_1?: number
  data_idle_reconnects_baseline_2?: number
  client_recovery_a_ms?: number
  client_recovery_b_ms?: number
  client_recovery_baseline_1_ms?: number
  client_recovery_baseline_2_ms?: number
  event_time_a?: StreamEventTime
  event_time_b?: StreamEventTime
  event_time_baseline_1?: StreamEventTime
  event_time_baseline_2?: StreamEventTime
  comparison?: ComparisonEligibility
  comparisons?: PairwiseComparisons
  watermark_skew_threshold_us?: number
}

export interface AvailabilityHistory {
  transport_up_seconds: number
  transport_down_seconds: number
  delivery_up_seconds: number
  delivery_down_seconds: number
  reconnect_reasons: Record<string, number>
  client_recovery_ms: number
  coverage: string
}

export interface ReliabilityHistory {
  stream_a: AvailabilityHistory
  stream_b: AvailabilityHistory
  baseline_1: AvailabilityHistory
  baseline_2: AvailabilityHistory
  event_time?: {
    stream_a: StreamEventTime
    stream_b: StreamEventTime
    baseline_1: StreamEventTime
    baseline_2: StreamEventTime
    comparison: ComparisonEligibility
    comparisons?: PairwiseComparisons
  }
}

interface UptimeHistoryResponse {
  data?: unknown
  rows?: unknown
  span_seconds?: unknown
  requested_window_seconds?: unknown
  interval_seconds?: unknown
  spanSeconds?: unknown
  requestedWindowSeconds?: unknown
  intervalSeconds?: unknown
}

function readNumber(value: unknown, fallback: number): number {
  if (typeof value === 'number' && Number.isFinite(value)) {
    return value
  }
  if (typeof value === 'string') {
    const parsed = Number(value)
    if (Number.isFinite(parsed)) {
      return parsed
    }
  }
  return fallback
}

function readString(value: unknown): string | null {
  if (typeof value !== 'string') {
    return null
  }
  const trimmed = value.trim()
  return trimmed.length > 0 ? trimmed : null
}

function readObject(value: unknown): Record<string, unknown> | null {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    return null
  }
  return value as Record<string, unknown>
}

function pickNumber(record: Record<string, unknown>, keys: string[]): number {
  for (const key of keys) {
    const value = readNumber(record[key], Number.NaN)
    if (Number.isFinite(value)) {
      return value
    }
  }
  return 0
}

export function normalizeUptimeRow(value: unknown): HourlyUptime | null {
  const row = readObject(value)
  if (!row) {
    return null
  }

  const hour = readString(row.hour) ?? readString(row.timestamp)
  if (!hour) {
    return null
  }

  let reliability: ReliabilityHistory | null = null
  const rawReliability = row.reliability_json ?? row.reliability
  if (typeof rawReliability === 'string' && rawReliability.trim()) {
    try { reliability = JSON.parse(rawReliability) as ReliabilityHistory } catch { reliability = null }
  } else if (readObject(rawReliability)) {
    reliability = rawReliability as ReliabilityHistory
  }

  return {
    hour,
    stream_a_seconds: reliability?.stream_a.transport_up_seconds ?? pickNumber(row, ['stream_a_seconds', 'uptime_a_seconds']),
    stream_b_seconds: reliability?.stream_b.transport_up_seconds ?? pickNumber(row, ['stream_b_seconds', 'uptime_b_seconds']),
    stream_a_downtime_seconds: reliability?.stream_a.transport_down_seconds ?? pickNumber(row, ['stream_a_downtime_seconds', 'downtime_a_seconds']),
    stream_b_downtime_seconds: reliability?.stream_b.transport_down_seconds ?? pickNumber(row, ['stream_b_downtime_seconds', 'downtime_b_seconds']),
    stream_a_disconnects: pickNumber(row, ['stream_a_disconnects', 'disconnects_a']),
    stream_b_disconnects: pickNumber(row, ['stream_b_disconnects', 'disconnects_b']),
    stream_a_messages: pickNumber(row, ['stream_a_messages', 'messages_a']),
    stream_b_messages: pickNumber(row, ['stream_b_messages', 'messages_b']),
    baseline_1_seconds: reliability?.baseline_1.transport_up_seconds ?? pickNumber(row, ['baseline_1_seconds']),
    baseline_2_seconds: reliability?.baseline_2.transport_up_seconds ?? pickNumber(row, ['baseline_2_seconds']),
    baseline_1_downtime_seconds: reliability?.baseline_1.transport_down_seconds ?? pickNumber(row, ['baseline_1_downtime_seconds']),
    baseline_2_downtime_seconds: reliability?.baseline_2.transport_down_seconds ?? pickNumber(row, ['baseline_2_downtime_seconds']),
    baseline_1_messages: pickNumber(row, ['baseline_1_messages']),
    baseline_2_messages: pickNumber(row, ['baseline_2_messages']),
    reliability,
    reliability_classification: readString(row.reliability_classification)
      ?? (reliability?.event_time?.comparisons ? 'observed' : 'legacy_unknown'),
  }
}

export function extractUptimeRows(response: unknown): HourlyUptime[] {
  if (Array.isArray(response)) {
    return response
      .map(normalizeUptimeRow)
      .filter((row): row is HourlyUptime => row !== null)
  }

  const record = readObject(response)
  if (!record) {
    return []
  }

  if (Array.isArray(record.data)) {
    return record.data
      .map(normalizeUptimeRow)
      .filter((row): row is HourlyUptime => row !== null)
  }

  if (Array.isArray(record.rows)) {
    return record.rows
      .map(normalizeUptimeRow)
      .filter((row): row is HourlyUptime => row !== null)
  }

  return []
}

export function useWebSocket(
  url: string, 
  onMessage: (stats: StreamStats) => void,
  onConnectionChange?: (status: ConnectionStatus) => void
) {
  const wsRef = useRef<WebSocket | null>(null)
  const reconnectTimeoutRef = useRef<number>()

  const connect = useCallback(() => {
    onConnectionChange?.('connecting')
    const ws = new WebSocket(url)

    ws.onopen = () => {
      console.log('WebSocket connected')
      onConnectionChange?.('connected')
    }

    ws.onclose = () => {
      console.log('WebSocket disconnected, reconnecting...')
      onConnectionChange?.('disconnected')
      reconnectTimeoutRef.current = window.setTimeout(connect, 3000)
    }

    ws.onerror = (error) => {
      console.error('WebSocket error:', error)
      onConnectionChange?.('disconnected')
    }

    ws.onmessage = (event) => {
      const stats = JSON.parse(event.data) as StreamStats
      onMessage(stats)
    }

    wsRef.current = ws
  }, [url, onMessage, onConnectionChange])

  useEffect(() => {
    connect()

    return () => {
      if (reconnectTimeoutRef.current) {
        clearTimeout(reconnectTimeoutRef.current)
      }
      wsRef.current?.close()
    }
  }, [connect])

  return wsRef
}

export function useUptimeHistory(hours: number, refreshInterval: number = 60000) {
  const [data, setData] = useState<HourlyUptime[]>([])
  const [spanSeconds, setSpanSeconds] = useState(0)
  const [requestedWindowSeconds, setRequestedWindowSeconds] = useState(hours * 3600)
  const [intervalSeconds, setIntervalSeconds] = useState(3600)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [lastUpdatedAt, setLastUpdatedAt] = useState<number | null>(null)
  const activeRequestIdRef = useRef(0)
  const requestControllerRef = useRef<AbortController | null>(null)

  const fetchData = useCallback(async () => {
    const requestId = activeRequestIdRef.current + 1
    activeRequestIdRef.current = requestId
    requestControllerRef.current?.abort()
    const controller = new AbortController()
    requestControllerRef.current = controller

    setLoading(true)
    try {
      const res = await fetch(`/api/uptime?hours=${hours}`, {
        signal: controller.signal,
      })
      if (!res.ok) throw new Error('Failed to fetch')
      const json = (await res.json()) as UptimeHistoryResponse

      if (requestId !== activeRequestIdRef.current || controller.signal.aborted) {
        return
      }

      const normalizedRows = extractUptimeRows(json)
      const metadata = readObject(json)
      const inferredSpanSeconds = normalizedRows.length > 0 ? normalizedRows.length * 3600 : hours * 3600

      setData(normalizedRows)
      setSpanSeconds(
        Math.max(
          0,
          readNumber(
            metadata?.span_seconds ?? metadata?.spanSeconds,
            inferredSpanSeconds,
          ),
        ),
      )
      setRequestedWindowSeconds(
        Math.max(
          0,
          readNumber(
            metadata?.requested_window_seconds ?? metadata?.requestedWindowSeconds,
            hours * 3600,
          ),
        ),
      )
      setIntervalSeconds(
        Math.max(
          1,
          readNumber(
            metadata?.interval_seconds ?? metadata?.intervalSeconds,
            3600,
          ),
        ),
      )
      setError(null)
      setLastUpdatedAt(Date.now())
    } catch (e) {
      if (e instanceof DOMException && e.name === 'AbortError') {
        return
      }
      if (requestId !== activeRequestIdRef.current) {
        return
      }
      setError(e instanceof Error ? e.message : 'Unknown error')
    } finally {
      if (requestId === activeRequestIdRef.current) {
        setLoading(false)
      }
    }
  }, [hours])

  useEffect(() => {
    activeRequestIdRef.current += 1
    requestControllerRef.current?.abort()
    requestControllerRef.current = null

    setData([])
    setSpanSeconds(0)
    setRequestedWindowSeconds(hours * 3600)
    setIntervalSeconds(3600)
    setError(null)
    setLastUpdatedAt(null)
    setLoading(true)
  }, [hours])

  useEffect(() => {
    fetchData()
    const interval = setInterval(fetchData, refreshInterval)
    return () => {
      clearInterval(interval)
      activeRequestIdRef.current += 1
      requestControllerRef.current?.abort()
      requestControllerRef.current = null
    }
  }, [fetchData, refreshInterval])

  return {
    data,
    spanSeconds,
    requestedWindowSeconds,
    intervalSeconds,
    loading,
    error,
    isStale: error !== null && data.length > 0,
    lastUpdatedAt,
    refetch: fetchData,
  }
}

export interface HourlyUptime {
  hour: string
  stream_a_seconds: number
  stream_b_seconds: number
  stream_a_downtime_seconds: number
  stream_b_downtime_seconds: number
  stream_a_disconnects: number
  stream_b_disconnects: number
  stream_a_messages: number
  stream_b_messages: number
  baseline_1_seconds: number
  baseline_2_seconds: number
  baseline_1_downtime_seconds: number
  baseline_2_downtime_seconds: number
  baseline_1_messages: number
  baseline_2_messages: number
  reliability: ReliabilityHistory | null
  reliability_classification: string
}
