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

  const bgColor = isPositive
    ? "rgba(34, 197, 94, 0.08)"
    : isNegative
      ? "rgba(239, 68, 68, 0.08)"
      : "rgba(255, 255, 255, 0.02)";
  const borderColor = isPositive
    ? "rgba(34, 197, 94, 0.25)"
    : isNegative
      ? "rgba(239, 68, 68, 0.25)"
      : "#1f1f1f";
  const glowColor = isPositive
    ? "rgba(34, 197, 94, 0.2)"
    : isNegative
      ? "rgba(239, 68, 68, 0.2)"
      : "transparent";
  const textColor = isPositive ? "#22c55e" : isNegative ? "#ef4444" : "#525252";
  const caret = isPositive ? "▲" : isNegative ? "▼" : "–";
  const statusLabel = isPositive
    ? `${streamAName} LEADS`
    : isNegative
      ? `${streamBName} LEADS`
      : "TIED";

  return (
    <div
      className="delta-card w-full flex items-center justify-center p-2 px-4 bg-bg-card-hover rounded-sm border transition-all duration-300 hover:shadow-lg hover:-translate-y-0.5"
      style={{
        backgroundColor: bgColor,
        borderColor: borderColor,
        boxShadow: `0 0 40px ${glowColor}, inset 0 0 30px ${glowColor}`,
      }}
    >
      <div className="flex flex-col md:flex-row items-center justify-center md:justify-between text-center gap-1 md:gap-8 w-full">
        <span className="text-[0.6875rem] text-text-muted uppercase tracking-widest">
          Current Diff
        </span>
        <span
          className="text-[2.5rem] font-bold tabular-nums flex items-center gap-2 transition-colors duration-200 leading-none"
          style={{
            color: textColor,
            textShadow: `0 0 30px ${glowColor}`,
          }}
        >
          <span className="text-[1.5rem] leading-none">{caret}</span>
          <span>{Math.abs(delta).toLocaleString()}</span>
        </span>
        <span
          className="text-[0.625rem] tracking-[0.2em] uppercase"
          style={{ color: textColor }}
        >
          {statusLabel}
        </span>
      </div>
    </div>
  );
}
