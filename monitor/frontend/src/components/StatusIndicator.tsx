import { useState, useEffect } from 'react'

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
    <div 
      className={`status fixed bottom-6 right-6 px-4 py-2.5 rounded-full text-[0.75rem] bg-[#141414] border border-[#1f1f1f] flex items-center gap-2.5 uppercase tracking-wider shadow-lg transition-all duration-300 hover:border-[#2a2a2a] ${
        connected ? 'text-[#22c55e]' : 'text-[#ef4444]'
      }`}
    >
      <span 
        className={`w-2 h-2 rounded-full ${
          connected 
            ? 'bg-[#22c55e] shadow-[0_0_8px_#22c55e] animate-pulse' 
            : 'bg-[#ef4444]'
        }`}
      />
      <span>{connected ? 'Connected' : 'Disconnected'}</span>
      {connected && duration && <span className="text-[#525252]">•</span>}
      {connected && duration && <span className="text-[#525252] normal-case tracking-normal">{duration}</span>}
    </div>
  )
}
