interface HeaderProps {
  title?: string
  subtitle?: string
}

export function Header({ title = 'Stream Monitor', subtitle = 'Real-time Bluesky Jetstream monitoring' }: HeaderProps) {
  return (
    <div className="text-center mb-14">
      <h1 className="text-4xl font-semibold tracking-tight mb-3 text-foreground">{title}</h1>
      <p className="text-lg text-muted-foreground">{subtitle}</p>
    </div>
  )
}
