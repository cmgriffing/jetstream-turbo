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
  let disconnectsA = 0;
  let disconnectsB = 0;

  data.forEach((row) => {
    a.uptimeSeconds += toNonNegative(row.stream_a_seconds);
    a.downtimeSeconds += toNonNegative(row.stream_a_downtime_seconds);
    a.messages += toNonNegative(row.stream_a_messages);
    b.uptimeSeconds += toNonNegative(row.stream_b_seconds);
    b.downtimeSeconds += toNonNegative(row.stream_b_downtime_seconds);
    b.messages += toNonNegative(row.stream_b_messages);
    baseline1.uptimeSeconds += toNonNegative(row.baseline_1_seconds);
    baseline1.downtimeSeconds += toNonNegative(row.baseline_1_downtime_seconds);
    baseline1.messages += toNonNegative(row.baseline_1_messages);
    baseline2.uptimeSeconds += toNonNegative(row.baseline_2_seconds);
    baseline2.downtimeSeconds += toNonNegative(row.baseline_2_downtime_seconds);
    baseline2.messages += toNonNegative(row.baseline_2_messages);
    disconnectsA += toNonNegative(row.stream_a_disconnects);
    disconnectsB += toNonNegative(row.stream_b_disconnects);
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
                Uptime ({windowLabel} window)
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
