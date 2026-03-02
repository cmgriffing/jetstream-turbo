interface StreamCardProps {
  streamId: 'a' | 'b'
  name: string
  count: number
  rate: number
  streak?: number
  uptime?: number
  connected: boolean
}

function formatDuration(ms: number): string {
  const secs = Math.floor(ms / 1000)
  const hrs = Math.floor(secs / 3600)
  const mins = Math.floor((secs % 3600) / 60)
  if (hrs > 0) return `${hrs}h ${mins}m`
  if (mins > 0) return `${mins}m ${secs % 60}s`
  return `${secs}s`
}

export function StreamCard({ streamId, name, count, rate, streak, uptime, connected }: StreamCardProps) {
  const streamClass = streamId === 'a' ? 'stream-a' : 'stream-b'
  const accentColor = streamId === 'a' ? '#22c55e' : '#3b82f6'
  const glowColor = streamId === 'a' ? 'rgba(34, 197, 94, 0.15)' : 'rgba(59, 130, 246, 0.15)'

  return (
    <div className={`stream-card ${streamClass} flex-1 bg-[#141414] rounded-2xl p-6 border border-[#1f1f1f] relative overflow-hidden transition-all duration-300 hover:border-[#2a2a2a] hover:shadow-[0_8px_32px_rgba(0,0,0,0.4)] hover:translate-y-[-2px]`}>
      <div 
        className="absolute left-0 top-0 bottom-0 w-1.5"
        style={{ 
          background: `linear-gradient(180deg, ${accentColor} 0%, ${accentColor}66 100%)`,
          boxShadow: `0 0 20px ${glowColor}`
        }}
      />
      <div 
        className="absolute inset-0 opacity-0 hover:opacity-100 transition-opacity duration-500 pointer-events-none"
        style={{ 
          background: `radial-gradient(ellipse at top left, ${glowColor} 0%, transparent 70%)`
        }}
      />
      <div className="flex justify-between items-center mb-5 pl-3">
        <span className="text-[0.8125rem] font-semibold uppercase tracking-wider" style={{ color: accentColor }}>
          {name}
        </span>
        <span className={`connection-badge text-[0.6875rem] px-2.5 py-1 rounded-full uppercase tracking-wider flex items-center gap-1.5 transition-colors duration-200 ${
          connected 
            ? 'bg-[rgba(34,197,94,0.12)] text-[#22c55e]' 
            : 'bg-[rgba(239,68,68,0.12)] text-[#ef4444]'
        }`}>
          <span className={`w-1.5 h-1.5 rounded-full ${connected ? 'bg-[#22c55e] shadow-[0_0_6px_#22c55e]' : 'bg-[#ef4444]'}`} />
          {connected ? 'Connected' : 'Disconnected'}
        </span>
      </div>
      <div className="text-[3.25rem] font-bold tabular-nums tracking-tight leading-none mb-1 pl-3">
        {count.toLocaleString()}
      </div>
      <div className="text-[0.75rem] text-[#525252] uppercase tracking-wider mb-6 pl-3">messages</div>
      <div className="grid grid-cols-3 gap-3 pt-5 border-t border-[#1f1f1f]">
        <div className="text-left pl-1">
          <span className="text-[0.6875rem] text-[#525252] uppercase tracking-wider block mb-1">Rate</span>
          <span className="text-[1.0625rem] font-semibold tabular-nums text-[#e5e5e5]">{rate.toFixed(0)}/s</span>
        </div>
        <div className="text-left pl-1">
          <span className="text-[0.6875rem] text-[#525252] uppercase tracking-wider block mb-1">Streak</span>
          <span className="text-[1.0625rem] font-semibold tabular-nums text-[#e5e5e5]">
            {streak ? formatDuration(streak * 1000) : '-'}
          </span>
        </div>
        <div className="text-left pl-1">
          <span className="text-[0.6875rem] text-[#525252] uppercase tracking-wider block mb-1">Session Uptime</span>
          <span className="text-[1.0625rem] font-semibold tabular-nums text-[#e5e5e5]">
            {uptime ? `${uptime.toFixed(1)}%` : '-'}
          </span>
        </div>
      </div>
    </div>
  )
}
