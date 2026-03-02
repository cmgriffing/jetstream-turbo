interface DeltaCardProps {
  delta: number
}

export function DeltaCard({ delta }: DeltaCardProps) {
  const isPositive = delta > 0
  const isNegative = delta < 0
  
  const bgColor = isPositive ? 'rgba(34, 197, 94, 0.08)' : isNegative ? 'rgba(239, 68, 68, 0.08)' : 'rgba(255, 255, 255, 0.02)'
  const borderColor = isPositive ? 'rgba(34, 197, 94, 0.25)' : isNegative ? 'rgba(239, 68, 68, 0.25)' : '#1f1f1f'
  const glowColor = isPositive ? 'rgba(34, 197, 94, 0.15)' : isNegative ? 'rgba(239, 68, 68, 0.15)' : 'transparent'
  const textColor = isPositive ? '#22c55e' : isNegative ? '#ef4444' : '#525252'
  const arrow = isPositive ? '↑' : isNegative ? '↓' : '–'

  return (
    <div 
      className="delta-card flex flex-col items-center justify-center p-5 bg-[#141414] rounded-2xl border min-w-[110px] transition-all duration-300 hover:shadow-lg hover:translate-y-[-2px]"
      style={{ 
        backgroundColor: bgColor,
        borderColor: borderColor,
        boxShadow: `0 0 24px ${glowColor}, inset 0 0 24px ${glowColor}`
      }}
    >
      <span className="text-[0.6875rem] text-[#525252] uppercase tracking-widest mb-2">Delta</span>
      <span 
        className="text-[2rem] font-bold tabular-nums flex items-center gap-1.5 transition-colors duration-200"
        style={{ color: textColor }}
      >
        <span className="text-[1.25rem] leading-none">{arrow}</span>
        <span>{Math.abs(delta).toLocaleString()}</span>
      </span>
    </div>
  )
}
