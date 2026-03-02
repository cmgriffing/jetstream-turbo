interface DeltaCardProps {
  delta: number
}

export function DeltaCard({ delta }: DeltaCardProps) {
  const isPositive = delta > 0
  const isNegative = delta < 0
  
  const bgColor = isPositive ? 'rgba(34, 197, 94, 0.08)' : isNegative ? 'rgba(239, 68, 68, 0.08)' : 'rgba(255, 255, 255, 0.02)'
  const borderColor = isPositive ? 'rgba(34, 197, 94, 0.25)' : isNegative ? 'rgba(239, 68, 68, 0.25)' : '#1f1f1f'
  const glowColor = isPositive ? 'rgba(34, 197, 94, 0.2)' : isNegative ? 'rgba(239, 68, 68, 0.2)' : 'transparent'
  const textColor = isPositive ? '#22c55e' : isNegative ? '#ef4444' : '#525252'
  const arrow = isPositive ? '↑' : isNegative ? '↓' : '–'

  return (
    <div 
      className="delta-card flex flex-col items-center justify-center p-4 md:p-6 bg-[#141414] rounded-3xl border min-w-[100px] max-w-[140px] transition-all duration-300 hover:shadow-lg hover:translate-y-[-2px]"
      style={{ 
        backgroundColor: bgColor,
        borderColor: borderColor,
        boxShadow: `0 0 40px ${glowColor}, inset 0 0 30px ${glowColor}`
      }}
    >
      <span className="text-[0.6875rem] text-[#525252] uppercase tracking-widest mb-3">Delta</span>
      <span 
        className="text-[2.5rem] font-bold tabular-nums flex items-center gap-2 transition-colors duration-200"
        style={{ 
          color: textColor,
          textShadow: `0 0 30px ${glowColor}`
        }}
      >
        <span className="text-[1.5rem] leading-none">{arrow}</span>
        <span>{Math.abs(delta).toLocaleString()}</span>
      </span>
    </div>
  )
}
