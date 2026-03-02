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
    <div className={`stream-card ${streamClass} flex-1 min-w-[280px] max-w-[400px] bg-[#141414] rounded-3xl p-6 md:p-8 border border-[#1f1f1f] relative overflow-hidden transition-all duration-300 hover:border-[#2a2a2a] hover:shadow-[0_8px_32px_rgba(0,0,0,0.4)] hover:translate-y-[-2px]`}>
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
      <div className="flex justify-between items-center mb-6 pl-4">
        <span className="text-[0.8125rem] font-semibold uppercase tracking-wider" style={{ color: accentColor }}>
          {name}
        </span>
        <span className={`connection-badge text-[0.6875rem] px-3 py-1.5 rounded-full uppercase tracking-wider flex items-center gap-1.5 transition-colors duration-200 ${
          connected 
            ? 'bg-[rgba(34,197,94,0.12)] text-[#22c55e]' 
            : 'bg-[rgba(239,68,68,0.12)] text-[#ef4444]'
        }`}>
          <span className={`w-1.5 h-1.5 rounded-full ${connected ? 'bg-[#22c55e] shadow-[0_0_6px_#22c55e]' : 'bg-[#ef4444]'}`} />
          {connected ? 'Connected' : 'Disconnected'}
        </span>
      </div>
      
      <div className="mb-2 pl-4">
        <div 
          className="inline-block text-[4rem] font-bold tabular-nums tracking-tight leading-none"
          style={{ 
            color: '#fafafa',
            textShadow: `0 0 40px ${glowColor}, 0 2px 4px rgba(0,0,0,0.3)`
          }}
        >
          {count.toLocaleString()}
        </div>
      </div>
      <div className="text-[0.8125rem] text-[#525252] uppercase tracking-wider mb-8 pl-4 flex items-center gap-2">
        <svg className="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={1.5}>
          <path strokeLinecap="round" strokeLinejoin="round" d="M20.25 8.511c.884.284 1.5 1.128 1.5 2.097v4.286c0 1.136-.847 2.1-1.98 2.193-.34.027-.68.052-1.02.072v3.091l-3-3c-1.354 0-2.694-.055-4.02-.163a2.115 2.115 0 01-.825-.242m9.345-8.334a2.126 2.126 0 00-.476-.095 48.64 48.64 0 00-8.048 0c-1.131.094-1.976 1.057-1.976 2.192v4.286c0 .837.46 1.58 1.155 1.951m9.345-8.334V6.637c0-1.621-1.152-3.026-2.76-3.235A48.455 48.455 0 0011.25 3c-2.115 0-4.198.137-6.24.402-1.608.209-2.76 1.614-2.76 3.235v6.226c0 1.621 1.152 3.026 2.76 3.235.577.075 1.157.14 1.74.194V21l4.155-4.155" />
        </svg>
        messages
      </div>
      
      <div className="grid grid-cols-3 gap-4 pt-6 border-t border-[#1f1f1f]">
        <div className="text-left pl-2">
          <span className="text-[0.6875rem] text-[#525252] uppercase tracking-wider block mb-2">Rate</span>
          <span className="text-[1.125rem] font-semibold tabular-nums text-[#e5e5e5]">{rate.toFixed(0)}/s</span>
        </div>
        <div className="border-l border-[#1f1f1f] pl-4">
          <span className="text-[0.6875rem] text-[#525252] uppercase tracking-wider block mb-2">Streak</span>
          <span className="text-[1.125rem] font-semibold tabular-nums text-[#e5e5e5]">
            {streak ? formatDuration(streak * 1000) : '-'}
          </span>
        </div>
        <div className="border-l border-[#1f1f1f] pl-4">
          <span className="text-[0.6875rem] text-[#525252] uppercase tracking-wider block mb-2">Session Uptime</span>
          <span className="text-[1.125rem] font-semibold tabular-nums text-[#e5e5e5]">
            {uptime ? `${uptime.toFixed(1)}%` : '-'}
          </span>
        </div>
      </div>
    </div>
  )
}
