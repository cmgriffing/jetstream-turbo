import { useState, useEffect, useRef, useCallback } from 'react'

export type ConnectionStatus = 'connecting' | 'connected' | 'disconnected'

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
  delivery_latency_a_ms?: number
  delivery_latency_b_ms?: number
  mttr_a_ms?: number
  mttr_b_ms?: number
}

interface UptimeHistoryResponse {
  data?: HourlyUptime[]
  span_seconds?: number
  requested_window_seconds?: number
  interval_seconds?: number
}

function readNumber(value: unknown, fallback: number): number {
  if (typeof value === 'number' && Number.isFinite(value)) {
    return value
  }
  return fallback
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

      setData(Array.isArray(json.data) ? json.data : [])
      setSpanSeconds(readNumber(json.span_seconds, hours * 3600))
      setRequestedWindowSeconds(readNumber(json.requested_window_seconds, hours * 3600))
      setIntervalSeconds(Math.max(1, readNumber(json.interval_seconds, 3600)))
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
  stream_a_delivery_latency_ms: number
  stream_b_delivery_latency_ms: number
  stream_a_mttr_ms: number
  stream_b_mttr_ms: number
}
