interface HeaderProps {
  title?: string
  subtitle?: string
}

export function Header({ title = 'Stream Monitor', subtitle = 'Real-time Bluesky Jetstream monitoring' }: HeaderProps) {
  return (
    <div className="header text-center mb-14">
      <h1 className="text-[2rem] font-semibold tracking-tight mb-3 bg-gradient-to-b from-[#fafafa] to-[#a3a3a3] bg-clip-text text-transparent">{title}</h1>
      <p className="text-[#737373] text-[1rem] font-light">{subtitle}</p>
    </div>
  )
}
