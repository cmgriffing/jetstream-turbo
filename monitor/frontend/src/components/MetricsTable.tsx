import { HourlyUptime } from "../hooks/useStream";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { cn } from "@/lib/utils";
import { formatUptimePercent } from "@/lib/uptime";

type TableRenderState = "loading" | "no_data" | "stale" | "disconnected" | "ready";

interface MetricsTableProps {
  title: string;
  icon: string;
  data: HourlyUptime[];
  spanSeconds: number;
  requestedWindowSeconds: number;
  intervalSeconds: number;
  renderState: TableRenderState;
  streamAName: string;
  streamBName: string;
  baseline1Name?: string;
  baseline2Name?: string;
  windowLabel: string;
}

function toNonNegative(value: number | undefined): number {
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0) {
    return 0;
  }
  return value;
}

function clampPercent(value: number): number {
  if (!Number.isFinite(value)) return 0;
  return Math.max(0, Math.min(100, value));
}

function formatDurationLong(seconds: number): string {
  const rounded = Math.max(0, Math.floor(seconds));
  const hrs = Math.floor(rounded / 3600);
  const mins = Math.floor((rounded % 3600) / 60);
  const secs = Math.floor(rounded % 60);
  if (hrs > 0) return `${hrs}h ${mins}m ${secs}s`;
  if (mins > 0) return `${mins}m ${secs}s`;
  return `${secs}s`;
}

function getUptimeToneClass(percentage: number): string {
  if (percentage >= 99) return "monitor-table-value--good";
  if (percentage >= 95) return "monitor-table-value--warn";
  return "monitor-table-value--bad";
}

function getStateMessage(state: TableRenderState): string {
  switch (state) {
    case "loading":
      return "Loading selected window";
    case "stale":
      return "Stale data snapshot";
    case "disconnected":
      return "Transport disconnected";
    case "no_data":
      return "No data in selected window";
    default:
      return "";
  }
}

function getStateOverlayMessage(state: TableRenderState): string {
  switch (state) {
    case "loading":
      return "Refreshing selected analytics window";
    case "stale":
      return "Stale data: showing most recent successful window";
    case "disconnected":
      return "Transport disconnected: showing last known window";
    default:
      return "";
  }
}

interface StreamAccumulator {
  uptimeSeconds: number;
  downtimeSeconds: number;
  messages: number;
}

interface StreamStatsSummary {
  uptimePercent: number;
  uptimeSeconds: number;
  downtimeSeconds: number;
  observedSeconds: number;
  messages: number;
  rate: number;
  coverage: number;
}

function summarizeStream(
  accumulator: StreamAccumulator,
  fallbackObservedSeconds: number,
  requestedWindow: number,
): StreamStatsSummary {
  const observedSeconds = accumulator.uptimeSeconds + accumulator.downtimeSeconds;
  const effectiveObserved = observedSeconds > 0 ? observedSeconds : fallbackObservedSeconds;
  return {
    uptimePercent:
      effectiveObserved > 0
        ? clampPercent((accumulator.uptimeSeconds / effectiveObserved) * 100.0)
        : 0,
    uptimeSeconds: accumulator.uptimeSeconds,
    downtimeSeconds: accumulator.downtimeSeconds,
    observedSeconds: effectiveObserved,
    messages: accumulator.messages,
    rate: effectiveObserved > 0 ? accumulator.messages / effectiveObserved : 0,
    coverage:
      requestedWindow > 0 ? clampPercent((effectiveObserved / requestedWindow) * 100) : 0,
  };
}

function calculateStats(
  data: HourlyUptime[],
  requestedWindowSeconds: number,
  spanSeconds: number,
  intervalSeconds: number,
) {
  if (data.length === 0) return null;

  const a: StreamAccumulator = { uptimeSeconds: 0, downtimeSeconds: 0, messages: 0 };
  const b: StreamAccumulator = { uptimeSeconds: 0, downtimeSeconds: 0, messages: 0 };
  const baseline1: StreamAccumulator = { uptimeSeconds: 0, downtimeSeconds: 0, messages: 0 };
  const baseline2: StreamAccumulator = { uptimeSeconds: 0, downtimeSeconds: 0, messages: 0 };
  const deliveryA: StreamAccumulator = { uptimeSeconds: 0, downtimeSeconds: 0, messages: 0 };
  const deliveryB: StreamAccumulator = { uptimeSeconds: 0, downtimeSeconds: 0, messages: 0 };
  const deliveryBaseline1: StreamAccumulator = { uptimeSeconds: 0, downtimeSeconds: 0, messages: 0 };
  const deliveryBaseline2: StreamAccumulator = { uptimeSeconds: 0, downtimeSeconds: 0, messages: 0 };
  const reasonTotals: Record<string, number> = {};
  let clientRecoveryMs = 0;
  let observedReliabilityRows = 0;
  let disconnectsA = 0;
  let disconnectsB = 0;

  data.forEach((row) => {
    const reliability = row.reliability;
    a.uptimeSeconds += toNonNegative(reliability?.stream_a.transport_up_seconds ?? row.stream_a_seconds);
    a.downtimeSeconds += toNonNegative(reliability?.stream_a.transport_down_seconds ?? row.stream_a_downtime_seconds);
    a.messages += toNonNegative(row.stream_a_messages);
    b.uptimeSeconds += toNonNegative(reliability?.stream_b.transport_up_seconds ?? row.stream_b_seconds);
    b.downtimeSeconds += toNonNegative(reliability?.stream_b.transport_down_seconds ?? row.stream_b_downtime_seconds);
    b.messages += toNonNegative(row.stream_b_messages);
    baseline1.uptimeSeconds += toNonNegative(reliability?.baseline_1.transport_up_seconds ?? row.baseline_1_seconds);
    baseline1.downtimeSeconds += toNonNegative(reliability?.baseline_1.transport_down_seconds ?? row.baseline_1_downtime_seconds);
    baseline1.messages += toNonNegative(row.baseline_1_messages);
    baseline2.uptimeSeconds += toNonNegative(reliability?.baseline_2.transport_up_seconds ?? row.baseline_2_seconds);
    baseline2.downtimeSeconds += toNonNegative(reliability?.baseline_2.transport_down_seconds ?? row.baseline_2_downtime_seconds);
    baseline2.messages += toNonNegative(row.baseline_2_messages);
    disconnectsA += toNonNegative(row.stream_a_disconnects);
    disconnectsB += toNonNegative(row.stream_b_disconnects);
    if (reliability) {
      observedReliabilityRows += 1;
      deliveryA.uptimeSeconds += toNonNegative(reliability.stream_a.delivery_up_seconds);
      deliveryA.downtimeSeconds += toNonNegative(reliability.stream_a.delivery_down_seconds);
      deliveryB.uptimeSeconds += toNonNegative(reliability.stream_b.delivery_up_seconds);
      deliveryB.downtimeSeconds += toNonNegative(reliability.stream_b.delivery_down_seconds);
      deliveryBaseline1.uptimeSeconds += toNonNegative(reliability.baseline_1.delivery_up_seconds);
      deliveryBaseline1.downtimeSeconds += toNonNegative(reliability.baseline_1.delivery_down_seconds);
      deliveryBaseline2.uptimeSeconds += toNonNegative(reliability.baseline_2.delivery_up_seconds);
      deliveryBaseline2.downtimeSeconds += toNonNegative(reliability.baseline_2.delivery_down_seconds);
      for (const stream of [reliability.stream_a, reliability.stream_b, reliability.baseline_1, reliability.baseline_2]) {
        clientRecoveryMs += toNonNegative(stream.client_recovery_ms);
        Object.entries(stream.reconnect_reasons).forEach(([reason, count]) => {
          reasonTotals[reason] = (reasonTotals[reason] ?? 0) + toNonNegative(count);
        });
      }
    }
  });

  const fallbackObservedSeconds = Math.max(
    0,
    spanSeconds,
    data.length * Math.max(1, intervalSeconds),
  );
  const requestedWindow = Math.max(0, requestedWindowSeconds);

  return {
    a: summarizeStream(a, fallbackObservedSeconds, requestedWindow),
    b: summarizeStream(b, fallbackObservedSeconds, requestedWindow),
    baseline1: summarizeStream(baseline1, fallbackObservedSeconds, requestedWindow),
    baseline2: summarizeStream(baseline2, fallbackObservedSeconds, requestedWindow),
    deliveryA: summarizeStream(deliveryA, 0, requestedWindow),
    deliveryB: summarizeStream(deliveryB, 0, requestedWindow),
    deliveryBaseline1: summarizeStream(deliveryBaseline1, 0, requestedWindow),
    deliveryBaseline2: summarizeStream(deliveryBaseline2, 0, requestedWindow),
    reliabilityCoverage: data.length > 0 ? (observedReliabilityRows / data.length) * 100 : 0,
    reasonTotals,
    clientRecoveryMs,
    disconnectsA,
    disconnectsB,
    requestedWindow,
  };
}

export function MetricsTable({
  title,
  icon,
  data,
  spanSeconds,
  requestedWindowSeconds,
  intervalSeconds,
  renderState,
  streamAName,
  streamBName,
  baseline1Name = "Baseline 1",
  baseline2Name = "Baseline 2",
  windowLabel,
}: MetricsTableProps) {
  const stats = calculateStats(data, requestedWindowSeconds, spanSeconds, intervalSeconds);
  const overlayMessage = getStateOverlayMessage(renderState);

  if (!stats) {
    return (
      <Card className="monitor-panel monitor-table-card" data-render-state={renderState}>
        <CardHeader className="monitor-table-card-header">
          <CardTitle className="monitor-chart-title">
            {title.toUpperCase().replace(/\s+/g, "_")}
          </CardTitle>
        </CardHeader>
        <CardContent className="monitor-table-card-content">
          <p className="monitor-table-state">{getStateMessage(renderState)}</p>
        </CardContent>
      </Card>
    );
  }

  return (
    <Card className="monitor-panel monitor-table-card" data-render-state={renderState}>
      <CardHeader className="monitor-table-card-header">
        <CardTitle className="monitor-chart-title">
          {title.toUpperCase().replace(/\s+/g, "_")}
          <span className="ml-2 monitor-table-head">{icon}</span>
        </CardTitle>
      </CardHeader>
      <CardContent className="monitor-table-card-content">
        {overlayMessage && (
          <div
            className={cn(
              "monitor-state-overlay",
              `monitor-state-overlay--${renderState}`,
            )}
          >
            {overlayMessage}
          </div>
        )}

        <Table className="monitor-metrics-table">
          <TableHeader>
            <TableRow className="monitor-metrics-head-row hover:bg-transparent">
              <TableHead className="monitor-table-head whitespace-normal">Metric</TableHead>
              <TableHead className="monitor-table-head text-right whitespace-normal break-words">
                {streamAName}
              </TableHead>
              <TableHead className="monitor-table-head text-right whitespace-normal break-words">
                {streamBName}
              </TableHead>
              <TableHead className="monitor-table-head text-right whitespace-normal break-words">
                {baseline1Name}
              </TableHead>
              <TableHead className="monitor-table-head text-right whitespace-normal break-words">
                {baseline2Name}
              </TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            <TableRow className="monitor-metrics-row">
              <TableCell className="monitor-table-label whitespace-normal">
                Transport uptime ({windowLabel} window)
              </TableCell>
              <TableCell
                className={cn(
                  "monitor-table-value monitor-table-value--numeric text-right whitespace-normal",
                  getUptimeToneClass(stats.a.uptimePercent),
                )}
              >
                {formatUptimePercent(stats.a.uptimePercent, { minimumFractionDigits: 2 })}%
              </TableCell>
              <TableCell
                className={cn(
                  "monitor-table-value monitor-table-value--numeric text-right whitespace-normal",
                  getUptimeToneClass(stats.b.uptimePercent),
                )}
              >
                {formatUptimePercent(stats.b.uptimePercent, { minimumFractionDigits: 2 })}%
              </TableCell>
              <TableCell
                className={cn(
                  "monitor-table-value monitor-table-value--numeric text-right whitespace-normal",
                  getUptimeToneClass(stats.baseline1.uptimePercent),
                )}
              >
                {formatUptimePercent(stats.baseline1.uptimePercent, { minimumFractionDigits: 2 })}%
              </TableCell>
              <TableCell
                className={cn(
                  "monitor-table-value monitor-table-value--numeric text-right whitespace-normal",
                  getUptimeToneClass(stats.baseline2.uptimePercent),
                )}
              >
                {formatUptimePercent(stats.baseline2.uptimePercent, { minimumFractionDigits: 2 })}%
              </TableCell>
            </TableRow>

            <TableRow className="monitor-metrics-row">
              <TableCell className="monitor-table-label whitespace-normal">Delivery uptime ({windowLabel} window)</TableCell>
              {[stats.deliveryA, stats.deliveryB, stats.deliveryBaseline1, stats.deliveryBaseline2].map((stream, index) => (
                <TableCell key={index} className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                  {stats.reliabilityCoverage > 0 ? `${formatUptimePercent(stream.uptimePercent, { minimumFractionDigits: 2 })}%` : "Unknown"}
                </TableCell>
              ))}
            </TableRow>

            <TableRow className="monitor-metrics-row">
              <TableCell className="monitor-table-label whitespace-normal">Reliability coverage</TableCell>
              <TableCell colSpan={4} className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.reliabilityCoverage.toFixed(1)}% {stats.reliabilityCoverage < 100 ? "(legacy / unknown intervals present)" : ""}
              </TableCell>
            </TableRow>

            <TableRow className="monitor-metrics-row">
              <TableCell className="monitor-table-label whitespace-normal">Reconnect causes / client recovery</TableCell>
              <TableCell colSpan={4} className="monitor-table-value text-right whitespace-normal">
                {Object.keys(stats.reasonTotals).length ? Object.entries(stats.reasonTotals).map(([reason, count]) => `${reason}: ${count}`).join(", ") : "Unknown"}
                {stats.clientRecoveryMs > 0 ? ` · ${formatDurationLong(stats.clientRecoveryMs / 1000)} client recovery` : ""}
              </TableCell>
            </TableRow>

            <TableRow className="monitor-metrics-row">
              <TableCell className="monitor-table-label whitespace-normal">Observed Up</TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {formatDurationLong(stats.a.uptimeSeconds)}
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {formatDurationLong(stats.b.uptimeSeconds)}
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {formatDurationLong(stats.baseline1.uptimeSeconds)}
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {formatDurationLong(stats.baseline2.uptimeSeconds)}
              </TableCell>
            </TableRow>

            <TableRow className="monitor-metrics-row">
              <TableCell className="monitor-table-label whitespace-normal">Observed Down</TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {formatDurationLong(stats.a.downtimeSeconds)}
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {formatDurationLong(stats.b.downtimeSeconds)}
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {formatDurationLong(stats.baseline1.downtimeSeconds)}
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {formatDurationLong(stats.baseline2.downtimeSeconds)}
              </TableCell>
            </TableRow>

            <TableRow className="monitor-metrics-row">
              <TableCell className="monitor-table-label whitespace-normal">
                Coverage of {windowLabel} window
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.a.coverage.toFixed(1)}%
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.b.coverage.toFixed(1)}%
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.baseline1.coverage.toFixed(1)}%
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.baseline2.coverage.toFixed(1)}%
              </TableCell>
            </TableRow>

            <TableRow className="monitor-metrics-row">
              <TableCell className="monitor-table-label whitespace-normal">Rate</TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.a.rate.toFixed(2)}
                <span className="monitor-table-unit">/s</span>
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.b.rate.toFixed(2)}
                <span className="monitor-table-unit">/s</span>
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.baseline1.rate.toFixed(2)}
                <span className="monitor-table-unit">/s</span>
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.baseline2.rate.toFixed(2)}
                <span className="monitor-table-unit">/s</span>
              </TableCell>
            </TableRow>

            <TableRow className="monitor-metrics-row">
              <TableCell className="monitor-table-label whitespace-normal">Disconnects</TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.disconnectsA}
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.disconnectsB}
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                <span className="monitor-stream-metric-value--empty">--</span>
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                <span className="monitor-stream-metric-value--empty">--</span>
              </TableCell>
            </TableRow>

            <TableRow className="monitor-metrics-row">
              <TableCell className="monitor-table-label whitespace-normal">Messages</TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.a.messages.toLocaleString()}
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.b.messages.toLocaleString()}
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.baseline1.messages.toLocaleString()}
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.baseline2.messages.toLocaleString()}
              </TableCell>
            </TableRow>
          </TableBody>
        </Table>
      </CardContent>
    </Card>
  );
}
