import { useState, useEffect } from 'react'
import { Wifi, WifiOff } from 'lucide-react'
import { Badge } from '@/components/ui/badge'
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
    <div className="fixed bottom-6 right-6 flex items-center gap-2">
      <Badge 
        variant={connected ? "default" : "destructive"}
        className={cn(
          "flex items-center gap-1.5 uppercase tracking-wider",
          connected && "bg-green-600 hover:bg-green-600 dark:bg-green-500 dark:hover:bg-green-500"
        )}
      >
        {connected ? (
          <Wifi className="w-3 h-3" />
        ) : (
          <WifiOff className="w-3 h-3" />
        )}
        {connected ? 'Connected' : 'Disconnected'}
      </Badge>
      {connected && duration && (
        <span className="text-muted-foreground text-sm">{duration}</span>
      )}
    </div>
  )
}
