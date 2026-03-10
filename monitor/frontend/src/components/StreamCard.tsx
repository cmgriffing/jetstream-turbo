import { memo } from "react";
import { Info, Zap } from "lucide-react";
import { cn } from "@/lib/utils";

interface StreamCardProps {
  streamId: "a" | "b";
  name: string;
  count: number;
  countingStartedAt?: string;
  rate: number;
  streak?: number;
  uptime?: number;
  uptimeAllTime?: number;
  connected: boolean;
  deliveryLatencyMs?: number;
}

function formatDuration(ms: number): string {
  const secs = Math.floor(ms / 1000);
  const hrs = Math.floor(secs / 3600);
  const mins = Math.floor((secs % 3600) / 60);
  if (hrs > 0) return `${hrs}h ${mins}m`;
  if (mins > 0) return `${mins}m ${secs % 60}s`;
  return `${secs}s`;
}

function formatCountingStartedAt(timestamp?: string): string {
  if (!timestamp) return "--";

  const date = new Date(timestamp);
  if (Number.isNaN(date.getTime())) return "--";

  return date.toLocaleString(undefined, {
    month: "short",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
    timeZoneName: "short",
  });
}

export const StreamCard = memo(function StreamCard({
  streamId,
  name,
  count,
  countingStartedAt,
  rate,
  streak,
  uptimeAllTime,
  connected,
  deliveryLatencyMs,
}: StreamCardProps) {
  const streamVariantClass =
    streamId === "a" ? "monitor-stream-card--a" : "monitor-stream-card--b";

  return (
    <article className={cn("monitor-stream-card", streamVariantClass)}>
      <div className="monitor-stream-top">
        <div className="monitor-stream-identity">
          <p className="monitor-eyebrow">Stream</p>
          <p className="monitor-stream-name">{name}</p>
        </div>
        <span
          className={cn(
            "monitor-stream-status",
            connected
              ? "monitor-stream-status--connected"
              : "monitor-stream-status--disconnected",
          )}
        >
          <span className="monitor-stream-status-dot" aria-hidden="true" />
          {connected ? "Connected" : "Disconnected"}
        </span>
      </div>

      <div className="monitor-stream-main">
        <p className="monitor-stream-count">{count.toLocaleString()}</p>
        <p className="monitor-stream-subtext">
          <Zap className="mr-1 inline h-3 w-3" aria-hidden="true" />
          Total messages
        </p>
        <p className="monitor-stream-started">
          Since {formatCountingStartedAt(countingStartedAt)}
        </p>
      </div>

      <div className="monitor-stream-metrics">
        <div className="monitor-stream-metric">
          <p className="monitor-stream-metric-label">
            Rate
            <button
              type="button"
              className="monitor-tooltip-trigger relative inline-flex cursor-pointer"
              aria-label="More info about rate"
            >
              <Info className="h-2.5 w-2.5" aria-hidden="true" />
              <span className="monitor-tooltip">
                Messages per second over the last 10 seconds.
              </span>
            </button>
          </p>
          <p className="monitor-stream-metric-value">
            {rate.toFixed(0)}
            <span className="monitor-stream-metric-unit">/s</span>
          </p>
        </div>

        <div className="monitor-stream-metric">
          <p className="monitor-stream-metric-label">
            Latency
            <button
              type="button"
              className="monitor-tooltip-trigger relative inline-flex cursor-pointer"
              aria-label="More info about delivery latency"
            >
              <Info className="h-2.5 w-2.5" aria-hidden="true" />
              <span className="monitor-tooltip">
                Delivery latency average measured over the latest 10-second interval.
              </span>
            </button>
          </p>
          <p className="monitor-stream-metric-value">
            {deliveryLatencyMs !== undefined && deliveryLatencyMs > 0 ? (
              <>
                {deliveryLatencyMs < 1000
                  ? deliveryLatencyMs.toFixed(0)
                  : `${(deliveryLatencyMs / 1000).toFixed(1)}s`}
                <span className="monitor-stream-metric-unit">
                  {deliveryLatencyMs < 1000 ? "ms" : ""}
                </span>
              </>
            ) : (
              <span className="monitor-stream-metric-value--empty">--</span>
            )}
          </p>
        </div>

        <div className="monitor-stream-metric">
          <p className="monitor-stream-metric-label">Streak</p>
          <p className="monitor-stream-metric-value">
            {streak ? (
              formatDuration(streak * 1000)
            ) : (
              <span className="monitor-stream-metric-value--empty">--</span>
            )}
          </p>
        </div>

        <div className="monitor-stream-metric">
          <p className="monitor-stream-metric-label">Uptime</p>
          <p className="monitor-stream-metric-value">
            {uptimeAllTime !== undefined ? (
              <>
                {uptimeAllTime.toFixed(1)}
                <span className="monitor-stream-metric-unit">%</span>
              </>
            ) : (
              <span className="monitor-stream-metric-value--empty">--</span>
            )}
          </p>
        </div>
      </div>
    </article>
  );
});
