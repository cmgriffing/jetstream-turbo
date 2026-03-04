interface HeaderProps {
  title?: string
  subtitle?: string
}

export function Header({ title = 'Stream Monitor', subtitle = 'Real-time Bluesky Jetstream monitoring' }: HeaderProps) {
  return (
    <div className="text-center mb-16 relative">
      <div className="absolute left-0 top-1/2 -translate-y-1/2 text-[10px] font-mono text-[#3fb950] opacity-60 hidden md:block">
        <span className="text-[#525252]">●</span> RUNNING
      </div>
      <h1 className="text-5xl font-semibold tracking-tight mb-3 text-foreground font-[inherit]">{title}</h1>
      <p className="text-sm text-muted-foreground tracking-[0.2em] uppercase font-[inherit]">{subtitle}</p>
      <div className="absolute right-0 top-1/2 -translate-y-1/2 text-[10px] font-mono text-[#8a8a8a] opacity-60 hidden md:block">
        {new Date().toISOString().slice(0, 10)} <span className="text-[#3fb950]">●</span>
      </div>
    </div>
  )
}
