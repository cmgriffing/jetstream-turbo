import { useState, useEffect } from 'react'
import { Wifi, WifiOff } from 'lucide-react'
import { cn } from '@/lib/utils'

interface StatusIndicatorProps {
  connected: boolean
  connectedAt?: number | null
}

function formatDuration(ms: number): string {
  const secs = Math.floor(ms / 1000)
  const hrs = Math.floor(secs / 3600)
  const mins = Math.floor((secs % 3600) / 60)
  if (hrs > 0) return `${hrs}h ${mins}m`
  if (mins > 0) return `${mins}m ${secs % 60}s`
  return `${secs}s`
}

export function StatusIndicator({ connected, connectedAt }: StatusIndicatorProps) {
  const [duration, setDuration] = useState('')

  useEffect(() => {
    if (!connected || !connectedAt) {
      setDuration('')
      return
    }

    const updateDuration = () => {
      setDuration(formatDuration(Date.now() - connectedAt))
    }

    updateDuration()
    const interval = setInterval(updateDuration, 1000)
    return () => clearInterval(interval)
  }, [connected, connectedAt])

  return (
    <div className="fixed bottom-6 right-6 flex items-center gap-2 z-40">
      <div 
        className={cn(
          "flex items-center gap-1.5 px-2 py-1 rounded-sm border text-[10px] font-mono tracking-wider",
          connected 
            ? "bg-[#0f0f0f] border-[#1a1a1a] text-[#3fb950]" 
            : "bg-[#0f0f0f] border-[#1a1a1a] text-[#f85149]"
        )}
      >
        {connected ? (
          <Wifi className="w-3 h-3" aria-hidden="true" />
        ) : (
          <WifiOff className="w-3 h-3" aria-hidden="true" />
        )}
        {connected ? 'ONLINE' : 'OFFLINE'}
      </div>
      {connected && duration && (
        <span className="text-[#525252] text-[10px] font-mono">{duration}</span>
      )}
    </div>
  )
}
