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

function calculateStats(
  data: HourlyUptime[],
  requestedWindowSeconds: number,
  spanSeconds: number,
  intervalSeconds: number,
) {
  if (data.length === 0) return null;

  let uptimeASeconds = 0;
  let uptimeBSeconds = 0;
  let downtimeASeconds = 0;
  let downtimeBSeconds = 0;
  let disconnectsA = 0;
  let disconnectsB = 0;
  let messagesA = 0;
  let messagesB = 0;
  let deliveryLatencySumA = 0;
  let deliveryLatencySumB = 0;
  let deliveryLatencyCountA = 0;
  let deliveryLatencyCountB = 0;
  let mttrSumA = 0;
  let mttrSumB = 0;
  let mttrCountA = 0;
  let mttrCountB = 0;

  data.forEach((row) => {
    const rowUptimeA = toNonNegative(row.stream_a_seconds);
    const rowUptimeB = toNonNegative(row.stream_b_seconds);
    const rowDowntimeA = toNonNegative(row.stream_a_downtime_seconds);
    const rowDowntimeB = toNonNegative(row.stream_b_downtime_seconds);

    uptimeASeconds += rowUptimeA;
    uptimeBSeconds += rowUptimeB;
    downtimeASeconds += rowDowntimeA;
    downtimeBSeconds += rowDowntimeB;
    disconnectsA += toNonNegative(row.stream_a_disconnects);
    disconnectsB += toNonNegative(row.stream_b_disconnects);
    messagesA += toNonNegative(row.stream_a_messages);
    messagesB += toNonNegative(row.stream_b_messages);

    const latencyA = toNonNegative(row.stream_a_delivery_latency_ms);
    const latencyB = toNonNegative(row.stream_b_delivery_latency_ms);
    if (latencyA > 0) {
      deliveryLatencySumA += latencyA;
      deliveryLatencyCountA += 1;
    }
    if (latencyB > 0) {
      deliveryLatencySumB += latencyB;
      deliveryLatencyCountB += 1;
    }

    const mttrA = toNonNegative(row.stream_a_mttr_ms);
    const mttrB = toNonNegative(row.stream_b_mttr_ms);
    if (mttrA > 0) {
      mttrSumA += mttrA;
      mttrCountA += 1;
    }
    if (mttrB > 0) {
      mttrSumB += mttrB;
      mttrCountB += 1;
    }
  });

  const fallbackObservedSeconds = Math.max(
    0,
    spanSeconds,
    data.length * Math.max(1, intervalSeconds),
  );

  const observedASeconds = uptimeASeconds + downtimeASeconds;
  const observedBSeconds = uptimeBSeconds + downtimeBSeconds;
  const effectiveObservedA = observedASeconds > 0 ? observedASeconds : fallbackObservedSeconds;
  const effectiveObservedB = observedBSeconds > 0 ? observedBSeconds : fallbackObservedSeconds;
  const requestedWindow = Math.max(0, requestedWindowSeconds);

  const uptimeA = effectiveObservedA > 0
    ? clampPercent((uptimeASeconds / effectiveObservedA) * 100.0)
    : 0;
  const uptimeB = effectiveObservedB > 0
    ? clampPercent((uptimeBSeconds / effectiveObservedB) * 100.0)
    : 0;

  return {
    uptimeA,
    uptimeB,
    uptimeASeconds,
    uptimeBSeconds,
    downtimeASeconds,
    downtimeBSeconds,
    observedASeconds: effectiveObservedA,
    observedBSeconds: effectiveObservedB,
    requestedWindow,
    disconnectsA,
    disconnectsB,
    messagesA,
    messagesB,
    rateA: effectiveObservedA > 0 ? messagesA / effectiveObservedA : 0,
    rateB: effectiveObservedB > 0 ? messagesB / effectiveObservedB : 0,
    coverageA: requestedWindow > 0 ? clampPercent((effectiveObservedA / requestedWindow) * 100) : 0,
    coverageB: requestedWindow > 0 ? clampPercent((effectiveObservedB / requestedWindow) * 100) : 0,
    deliveryLatencyA: deliveryLatencyCountA > 0 ? deliveryLatencySumA / deliveryLatencyCountA : 0,
    deliveryLatencyB: deliveryLatencyCountB > 0 ? deliveryLatencySumB / deliveryLatencyCountB : 0,
    mttrA: mttrCountA > 0 ? mttrSumA / mttrCountA : 0,
    mttrB: mttrCountB > 0 ? mttrSumB / mttrCountB : 0,
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
              <TableHead className="monitor-table-head w-2/5 whitespace-normal">Metric</TableHead>
              <TableHead className="monitor-table-head w-3/10 text-right whitespace-normal break-words">
                {streamAName}
              </TableHead>
              <TableHead className="monitor-table-head w-3/10 text-right whitespace-normal break-words">
                {streamBName}
              </TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            <TableRow className="monitor-metrics-row">
              <TableCell className="monitor-table-label whitespace-normal">Uptime</TableCell>
              <TableCell
                className={cn(
                  "monitor-table-value monitor-table-value--numeric text-right whitespace-normal",
                  getUptimeToneClass(stats.uptimeA),
                )}
              >
                {stats.uptimeA.toFixed(2)}%
              </TableCell>
              <TableCell
                className={cn(
                  "monitor-table-value monitor-table-value--numeric text-right whitespace-normal",
                  getUptimeToneClass(stats.uptimeB),
                )}
              >
                {stats.uptimeB.toFixed(2)}%
              </TableCell>
            </TableRow>

            <TableRow className="monitor-metrics-row">
              <TableCell className="monitor-table-label whitespace-normal">Observed Up</TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {formatDurationLong(stats.uptimeASeconds)}
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {formatDurationLong(stats.uptimeBSeconds)}
              </TableCell>
            </TableRow>

            <TableRow className="monitor-metrics-row">
              <TableCell className="monitor-table-label whitespace-normal">Coverage {windowLabel}</TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.coverageA.toFixed(1)}%
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.coverageB.toFixed(1)}%
              </TableCell>
            </TableRow>

            <TableRow className="monitor-metrics-row">
              <TableCell className="monitor-table-label whitespace-normal">Rate</TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.rateA.toFixed(2)}
                <span className="monitor-table-unit">/s</span>
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.rateB.toFixed(2)}
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
            </TableRow>

            <TableRow className="monitor-metrics-row">
              <TableCell className="monitor-table-label whitespace-normal">Delivery Latency</TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.deliveryLatencyA > 0 ? `${stats.deliveryLatencyA.toFixed(0)}ms` : "--"}
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.deliveryLatencyB > 0 ? `${stats.deliveryLatencyB.toFixed(0)}ms` : "--"}
              </TableCell>
            </TableRow>

            <TableRow className="monitor-metrics-row">
              <TableCell className="monitor-table-label whitespace-normal">MTTR</TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.mttrA > 0 ? formatDurationLong(stats.mttrA / 1000) : "--"}
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.mttrB > 0 ? formatDurationLong(stats.mttrB / 1000) : "--"}
              </TableCell>
            </TableRow>

            <TableRow className="monitor-metrics-row">
              <TableCell className="monitor-table-label whitespace-normal">Messages</TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.messagesA.toLocaleString()}
              </TableCell>
              <TableCell className="monitor-table-value monitor-table-value--numeric text-right whitespace-normal">
                {stats.messagesB.toLocaleString()}
              </TableCell>
            </TableRow>
          </TableBody>
        </Table>
      </CardContent>
    </Card>
  );
}
