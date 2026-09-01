import { memo } from "react";
import { Info, Zap } from "lucide-react";
import { cn } from "@/lib/utils";
import { formatUptimePercent } from "@/lib/uptime";
import type { OrdinalAccounting, StreamEventTime } from "@/hooks/useStream";

interface StreamCardProps {
  streamId: "a" | "b" | "baseline-1" | "baseline-2";
  name: string;
  fullName?: string;
  count?: number;
  countingStartedAt?: string;
  rate: number;
  streak?: number;
  uptime?: number;
  uptimeAllTime?: number;
  connected: boolean;
  deliveryAvailable?: boolean;
  transportUptime?: number;
  deliveryUptime?: number;
  reconnectReason?: string;
  clientRecoveryMs?: number;
  eventTime?: StreamEventTime;
  /** Contract v4: connected-to-disconnected episodes. */
  outageEpisodes?: number;
  /** Contract v4: failed reconnect attempts, counted independently of episodes. */
  reconnectAttempts?: number;
  /** Contract v4: transport recovery events. */
  transportRecoveries?: number;
  /** Contract v4: delivery recovery events. */
  deliveryRecoveries?: number;
  /** Contract v4: delivery-idle episodes on live sockets. */
  idleEpisodes?: number;
  deliveryIdle?: boolean;
  /** Additive ingress-ordinal accounting (turbo-fed streams only). */
  ordinal?: OrdinalAccounting;
}

export function isThresholdBreached(ordinal?: OrdinalAccounting): boolean {
  return (
    ordinal !== undefined &&
    ordinal.status === "active" &&
    (ordinal.duplicate_ratio > 0.05 || ordinal.gap_rate > 0.005)
  );
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
  fullName,
  count,
  countingStartedAt,
  rate,
  streak,
  uptimeAllTime,
  connected,
  deliveryAvailable,
  transportUptime,
  deliveryUptime,
  reconnectReason,
  clientRecoveryMs,
  eventTime,
  outageEpisodes,
  reconnectAttempts,
  transportRecoveries,
  deliveryRecoveries,
  idleEpisodes,
  deliveryIdle,
  ordinal,
}: StreamCardProps) {
  const isBaseline = streamId === "baseline-1" || streamId === "baseline-2";
  const completeIdentity = fullName || name;
  const streamVariantClass =
    streamId === "a"
      ? "monitor-stream-card--a"
      : streamId === "b"
        ? "monitor-stream-card--b"
        : "monitor-stream-card--baseline";

  return (
    <article className={cn("monitor-stream-card", streamVariantClass)}>
      <div className="monitor-stream-top">
        <div className="monitor-stream-identity">
          <p className="monitor-eyebrow">{isBaseline ? "Reference feed" : "Primary feed"}</p>
          <p className="monitor-stream-name" title={completeIdentity}>{name}</p>
          {completeIdentity !== name ? (
            <p className="monitor-stream-full-name">{completeIdentity}</p>
          ) : null}
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
        <p className="monitor-stream-count">{count?.toLocaleString() ?? "—"}</p>
        <p className="monitor-stream-subtext">
          <Zap className="mr-1 inline h-3 w-3" aria-hidden="true" />
          Raw arrivals
        </p>
        <p className="monitor-stream-started">
          Since {formatCountingStartedAt(countingStartedAt)}
        </p>
      </div>

      <div className="monitor-stream-metrics">
        <div className="monitor-stream-metric">
          <p className="monitor-stream-metric-label">
            Observed arrival rate
            <button
              type="button"
              className="monitor-tooltip-trigger relative inline-flex cursor-pointer"
              aria-label="More info about rate"
            >
              <Info className="h-2.5 w-2.5" aria-hidden="true" />
              <span className="monitor-tooltip">
                Raw frames per second over the last 10 seconds. Catch-up delivery can exceed live source throughput.
              </span>
            </button>
          </p>
          <p className="monitor-stream-metric-value">
            {rate.toFixed(0)}
            <span className="monitor-stream-metric-unit">/s</span>
          </p>
        </div>

        <div className="monitor-stream-metric">
          <p className="monitor-stream-metric-label">Source delivery mode</p>
          <p className="monitor-stream-metric-value">
            {eventTime?.delivery_mode === "catching_up"
              ? "Catching up"
              : eventTime?.delivery_mode === "live"
                ? "Live"
                : "Unknown"}
            <span className="monitor-stream-metric-unit">
              {eventTime?.source_lag_us !== null && eventTime?.source_lag_us !== undefined
                ? `${(eventTime.source_lag_us / 1_000_000).toFixed(1)}s source lag`
                : "No source watermark"}
            </span>
          </p>
        </div>

        <div className="monitor-stream-metric">
          <p className="monitor-stream-metric-label">Event-time coverage</p>
          <p className="monitor-stream-metric-value">
            {eventTime?.event_time_coverage ? "Covered" : "Missing"}
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
          <p className="monitor-stream-metric-label">Transport uptime</p>
          <p className="monitor-stream-metric-value">
            {(transportUptime ?? uptimeAllTime) !== undefined ? (
              <>
                {formatUptimePercent(transportUptime ?? uptimeAllTime ?? 0, { minimumFractionDigits: 2 })}
                <span className="monitor-stream-metric-unit">%</span>
              </>
            ) : (
              <span className="monitor-stream-metric-value--empty">--</span>
            )}
          </p>
        </div>
        <div className="monitor-stream-metric">
          <p className="monitor-stream-metric-label">
            Delivery uptime
            <button type="button" className="monitor-tooltip-trigger relative inline-flex cursor-pointer" aria-label="More info about delivery uptime">
              <Info className="h-2.5 w-2.5" aria-hidden="true" />
              <span className="monitor-tooltip">Useful records arriving on time, independent of socket reachability.</span>
            </button>
          </p>
          <p className="monitor-stream-metric-value">
            {deliveryUptime !== undefined ? <>{formatUptimePercent(deliveryUptime, { minimumFractionDigits: 2 })}<span className="monitor-stream-metric-unit">%</span></> : <span className="monitor-stream-metric-value--empty">--</span>}
          </p>
        </div>
        <div className="monitor-stream-metric">
          <p className="monitor-stream-metric-label">Delivery state</p>
          <p className="monitor-stream-metric-value">{deliveryAvailable === undefined ? "Unknown" : deliveryAvailable ? "Delivering" : "Stale"}</p>
        </div>
        <div className="monitor-stream-metric">
          <p className="monitor-stream-metric-label">
            Delivery state
            <button
              type="button"
              className="monitor-tooltip-trigger relative inline-flex cursor-pointer"
              aria-label="More info about delivery state"
            >
              <Info className="h-2.5 w-2.5" aria-hidden="true" />
              <span className="monitor-tooltip">
                Delivery idle keeps the socket connected while waiting for records.
              </span>
            </button>
          </p>
          <p className="monitor-stream-metric-value">
            {deliveryIdle
              ? "Delivery idle"
              : deliveryAvailable === undefined
                ? "Unknown"
                : deliveryAvailable
                  ? "Delivering"
                  : "Stale"}
          </p>
        </div>
        <div className="monitor-stream-metric">
          <p className="monitor-stream-metric-label">
            Outage episodes
            <button
              type="button"
              className="monitor-tooltip-trigger relative inline-flex cursor-pointer"
              aria-label="More info about outage episodes"
            >
              <Info className="h-2.5 w-2.5" aria-hidden="true" />
              <span className="monitor-tooltip">
                One episode per connected-to-disconnected transition. Reconnect attempts are counted separately.
              </span>
            </button>
          </p>
          <p className="monitor-stream-metric-value">
            {outageEpisodes ?? "--"}
            {reconnectAttempts !== undefined ? (
              <span className="monitor-stream-metric-unit">
                {reconnectAttempts} reconnect attempt{reconnectAttempts === 1 ? "" : "s"}
              </span>
            ) : null}
          </p>
        </div>
        <div className="monitor-stream-metric">
          <p className="monitor-stream-metric-label">Transport / delivery recovery</p>
          <p className="monitor-stream-metric-value">
            {transportRecoveries !== undefined || deliveryRecoveries !== undefined ? (
              <>
                {transportRecoveries ?? 0}
                <span className="monitor-stream-metric-unit">transport</span>
                {deliveryRecoveries ?? 0}
                <span className="monitor-stream-metric-unit">delivery</span>
              </>
            ) : (
              <span className="monitor-stream-metric-value--empty">--</span>
            )}
            {idleEpisodes !== undefined ? (
              <span className="monitor-stream-metric-unit">{idleEpisodes} idle episode{idleEpisodes === 1 ? "" : "s"}</span>
            ) : null}
          </p>
        </div>
        <div className="monitor-stream-metric monitor-stream-metric--detail">
          <p className="monitor-stream-metric-label">Last transport outcome</p>
          <p className="monitor-stream-metric-value">
            {reconnectReason ? reconnectReason.replace(/_/g, " ") : "--"}
            {clientRecoveryMs !== undefined ? <span className="monitor-stream-metric-unit">Client recovery {formatDuration(clientRecoveryMs)}</span> : null}
          </p>
        </div>
        {ordinal !== undefined ? (
          <div className="monitor-stream-metric">
            <p className="monitor-stream-metric-label">
              Ordinal accounting ({ordinal.status})
              <button
                type="button"
                className="monitor-tooltip-trigger relative inline-flex cursor-pointer"
                aria-label="More info about ordinal accounting"
              >
                <Info className="h-2.5 w-3" aria-hidden="true" />
                <span className="monitor-tooltip">
                  Ingress-ordinal classification: unique vs duplicate delivery and synthetic gaps from missing ordinals. Raw frame counts above are unchanged.
                </span>
              </button>
            </p>
            <p
              className={cn(
                "monitor-stream-metric-value",
                isThresholdBreached(ordinal) && "monitor-stream-metric-value--breach",
              )}
            >
              {ordinal.unique_total.toLocaleString()}
              <span className="monitor-stream-metric-unit">unique</span>
              {ordinal.duplicate_total.toLocaleString()}
              <span className="monitor-stream-metric-unit">dup</span>
              {isThresholdBreached(ordinal) ? "⚠ breach" : null}
            </p>
            {ordinal.gap_total > 0 || ordinal.uninstrumented_total > 0 ? (
              <p className="monitor-stream-metric-unit">
                {ordinal.gap_total.toLocaleString()} gap · {ordinal.uninstrumented_total.toLocaleString()} uninstrumented
                {ordinal.turbo_epoch ? ` · epoch ${ordinal.turbo_epoch}` : ""}
              </p>
            ) : null}
          </div>
        ) : null}
      </div>
    </article>
  );
});
