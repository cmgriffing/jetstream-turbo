import { useState, useEffect, useRef, useCallback } from 'react'

export type ConnectionStatus = 'connecting' | 'connected' | 'disconnected'

export interface StreamStats {
  stream_a?: number
  stream_b?: number
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
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const fetchData = useCallback(async () => {
    try {
      const res = await fetch(`/api/uptime?hours=${hours}`)
      if (!res.ok) throw new Error('Failed to fetch')
      const json = await res.json()
      setData(json.data || [])
      setSpanSeconds(json.span_seconds || hours * 3600)
      setError(null)
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Unknown error')
    } finally {
      setLoading(false)
    }
  }, [hours])

  useEffect(() => {
    fetchData()
    const interval = setInterval(fetchData, refreshInterval)
    return () => clearInterval(interval)
  }, [fetchData, refreshInterval])

  return { data, spanSeconds, loading, error, refetch: fetchData }
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
}
