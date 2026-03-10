interface HeaderProps {
  title?: string;
  subtitle?: string;
}

export function Header({
  title = "Stream Monitor",
  subtitle = "Real-time Bluesky Jetstream monitoring",
}: HeaderProps) {
  const dateStamp = new Date().toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "2-digit",
  });

  return (
    <header className="monitor-panel monitor-header">
      <div className="monitor-header-grid">
        <div>
          <p className="monitor-eyebrow">Operations Console</p>
          <h1 className="monitor-header-title">{title}</h1>
          <p className="monitor-header-subtitle">{subtitle}</p>
        </div>

        <div className="monitor-header-meta" aria-label="Monitor header status">
          <span className="monitor-pill monitor-pill--good">Realtime</span>
          <span className="monitor-pill monitor-pill--neutral">{dateStamp}</span>
        </div>
      </div>
    </header>
  );
}
