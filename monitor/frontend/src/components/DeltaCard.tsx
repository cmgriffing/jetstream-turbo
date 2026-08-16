import { CountComparison } from "@/lib/comparison";

export interface BaselineComparison {
  subjectLabel: string;
  baselineLabel: string;
  baselineIdentity: string;
  comparison: CountComparison;
}

interface DeltaCardProps {
  primaryComparison: CountComparison;
  streamAName?: string;
  streamBName?: string;
  baselineComparisons: BaselineComparison[];
}

export function DeltaCard({
  primaryComparison,
  streamAName = "STREAM_A",
  streamBName = "STREAM_B",
  baselineComparisons,
}: DeltaCardProps) {
  const isPositive = primaryComparison.position === "ahead";
  const isNegative = primaryComparison.position === "behind";
  const isUnavailable = primaryComparison.position === "unavailable";

  const toneClass = isPositive
    ? "monitor-delta-card--lead-a"
    : isNegative
      ? "monitor-delta-card--lead-b"
      : isUnavailable
        ? "monitor-delta-card--unavailable"
        : "monitor-delta-card--tie";

  const valueClass = isPositive
    ? "monitor-delta-value--lead-a"
    : isNegative
      ? "monitor-delta-value--lead-b"
      : isUnavailable
        ? "monitor-delta-value--unavailable"
        : "monitor-delta-value--tie";

  const caret = isPositive ? "▲" : isNegative ? "▼" : isUnavailable ? "—" : "=";
  const statusLabel = isPositive
    ? `${streamAName} LEADS`
    : isNegative
      ? `${streamBName} LEADS`
      : isUnavailable
        ? "WAITING FOR BOTH COUNTS"
        : "EVEN";

  return (
    <div className={`monitor-delta-card ${toneClass}`} role="status" aria-live="polite">
      <div className="monitor-delta-grid">
        <div className="monitor-delta-head">
          <p className="monitor-delta-label">Primary comparison</p>
          <p className="monitor-delta-comparison">{streamAName} vs {streamBName}</p>
        </div>

        <p className={`monitor-delta-value ${valueClass}`}>
          <span>{caret}</span>
          <span>{primaryComparison.magnitude?.toLocaleString() ?? "Unavailable"}</span>
        </p>

        <p className="monitor-delta-status">{statusLabel}</p>
      </div>

      <div className="monitor-baseline-comparison" aria-label="Baseline-relative message positions">
        <div className="monitor-baseline-comparison-head">
          <p className="monitor-eyebrow">Reference position</p>
          <p>Signed message difference from each baseline</p>
        </div>
        <div className="monitor-baseline-matrix">
          {baselineComparisons.map(({ subjectLabel, baselineLabel, baselineIdentity, comparison }) => {
            const signedDifference = comparison.difference === null
              ? "—"
              : comparison.difference > 0
                ? `+${comparison.difference.toLocaleString()}`
                : comparison.difference.toLocaleString();

            return (
              <div
                className={`monitor-baseline-result monitor-baseline-result--${comparison.position}`}
                key={`${subjectLabel}-${baselineLabel}`}
              >
                <p className="monitor-baseline-result-label">
                  {subjectLabel} <span>vs</span> <span title={baselineIdentity}>{baselineLabel}</span>
                </p>
                <p className="monitor-baseline-result-value">{signedDifference}</p>
                <p className="monitor-baseline-result-state">
                  {comparison.position === "unavailable" ? "unavailable" : comparison.position}
                </p>
                <span className="sr-only">{baselineLabel} is {baselineIdentity}.</span>
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}
