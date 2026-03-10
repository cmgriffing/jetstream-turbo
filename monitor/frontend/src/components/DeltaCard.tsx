interface DeltaCardProps {
  delta: number;
  streamAName?: string;
  streamBName?: string;
}

export function DeltaCard({
  delta,
  streamAName = "STREAM_A",
  streamBName = "STREAM_B",
}: DeltaCardProps) {
  const isPositive = delta > 0;
  const isNegative = delta < 0;

  const toneClass = isPositive
    ? "monitor-delta-card--lead-a"
    : isNegative
      ? "monitor-delta-card--lead-b"
      : "monitor-delta-card--tie";

  const valueClass = isPositive
    ? "monitor-delta-value--lead-a"
    : isNegative
      ? "monitor-delta-value--lead-b"
      : "monitor-delta-value--tie";

  const caret = isPositive ? "▲" : isNegative ? "▼" : "=";
  const statusLabel = isPositive
    ? `${streamAName} LEADS`
    : isNegative
      ? `${streamBName} LEADS`
      : "TIED";

  return (
    <div className={`monitor-delta-card ${toneClass}`}>
      <div className="monitor-delta-grid">
        <p className="monitor-delta-label">Current Diff</p>

        <p className={`monitor-delta-value ${valueClass}`}>
          <span>{caret}</span>
          <span>{Math.abs(delta).toLocaleString()}</span>
        </p>

        <p className="monitor-delta-status">{statusLabel}</p>
      </div>
    </div>
  );
}
