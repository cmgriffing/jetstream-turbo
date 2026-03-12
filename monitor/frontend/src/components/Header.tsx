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
  const timeStamp = new Date().toLocaleTimeString(undefined, {
    hour: "2-digit",
    minute: "2-digit",
  });

  return (
    <header className="monitor-panel monitor-header monitor-header-shell">
      <div className="monitor-header-stripe" aria-hidden="true" />
      <div className="monitor-header-grid">
        <div className="monitor-header-copy monitor-header-copy--stack">
          <p className="monitor-eyebrow monitor-header-eyebrow">Operations Console</p>
          <h1 className="monitor-header-title">{title}</h1>
          <p className="monitor-header-subtitle">{subtitle}</p>
        </div>

        <div className="monitor-header-meta monitor-header-meta--status" aria-label="Monitor header status">
          <div className="monitor-header-pill-row">
            <span className="monitor-pill monitor-pill--good">Realtime</span>
            <span className="monitor-pill monitor-pill--neutral">{dateStamp}</span>
          </div>
          <p className="monitor-header-meta-note">{`Local ${timeStamp}`}</p>
        </div>
      </div>
    </header>
  );
}
