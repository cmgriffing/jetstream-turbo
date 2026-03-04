import { useState, useCallback } from "react"
import { Header } from "@/components/Header"
import { StreamCard } from "@/components/StreamCard"
import { MetricsTable } from "@/components/MetricsTable"
import { StatusIndicator } from "@/components/StatusIndicator"
import { ConnectionBanner } from "@/components/ConnectionBanner"
import { UptimeChart24h, RateChart } from "@/components/Charts"
import { StreamStats, useWebSocket, useUptimeHistory, ConnectionStatus } from "@/hooks/useStream"

function App() {
  const [stats, setStats] = useState<StreamStats>({})
  const [connectionStatus, setConnectionStatus] = useState<ConnectionStatus>('connecting')
  
  const handleMessage = useCallback((newStats: StreamStats) => {
    setStats(newStats)
  }, [])

  const wsProtocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
  const wsUrl = `${wsProtocol}//${window.location.host}/ws`
  
  useWebSocket(wsUrl, handleMessage, setConnectionStatus)

  const { data: hourlyData, spanSeconds } = useUptimeHistory(24)

  return (
    <div className="min-h-screen bg-background p-8 relative" style={{ zIndex: 1 }}>
      <ConnectionBanner status={connectionStatus} />
      
      <Header />
      
      <div className="flex gap-6 mb-10 justify-center flex-wrap">
        <StreamCard
          streamId="a"
          name={stats.stream_a_name || "STREAM_A"}
          count={stats.stream_a || 0}
          rate={stats.rate_a || 0}
          streak={stats.current_streak_a}
          uptimeAllTime={stats.uptime_a_all_time}
          connected={stats.connected_a || false}
        />
        <StreamCard
          streamId="b"
          name={stats.stream_b_name || "STREAM_B"}
          count={stats.stream_b || 0}
          rate={stats.rate_b || 0}
          streak={stats.current_streak_b}
          uptimeAllTime={stats.uptime_b_all_time}
          connected={stats.connected_b || false}
        />
      </div>

      <div className="max-w-4xl mx-auto space-y-6">
        <UptimeChart24h 
          data={hourlyData}
          streamAName={stats.stream_a_name || "Stream A"}
          streamBName={stats.stream_b_name || "Stream B"}
        />
        
        <RateChart 
          data={hourlyData}
          streamAName={stats.stream_a_name || "Stream A"}
          streamBName={stats.stream_b_name || "Stream B"}
        />
        
        <MetricsTable
          title="Last 24 Hours"
          icon="📊"
          data={hourlyData}
          spanSeconds={spanSeconds}
          streamAName={stats.stream_a_name || "Stream A"}
          streamBName={stats.stream_b_name || "Stream B"}
        />
      </div>

      <StatusIndicator 
        connected={stats.connected_a || stats.connected_b || false} 
        connectedAt={Date.now()}
      />
    </div>
  )
}

export default App