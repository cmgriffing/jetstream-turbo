import { useMemo, lazy, Suspense } from "react";
import {
  Chart as ChartJS,
  CategoryScale,
  LinearScale,
  BarElement,
  PointElement,
  LineElement,
  Tooltip,
  Legend,
} from "chart.js";
import { HourlyUptime } from "../hooks/useStream";

ChartJS.register(
  CategoryScale,
  LinearScale,
  BarElement,
  PointElement,
  LineElement,
  Tooltip,
  Legend,
);

const Bar = lazy(() => import("react-chartjs-2").then((m) => ({ default: m.Bar })));
const Line = lazy(() => import("react-chartjs-2").then((m) => ({ default: m.Line })));

export type ChartRenderState =
  | "loading"
  | "no_data"
  | "stale"
  | "disconnected"
  | "ready";

interface ChartProps {
  data: HourlyUptime[];
  streamAName: string;
  streamBName: string;
  renderState?: ChartRenderState;
  windowLabel?: string;
}

interface RateChartProps extends ChartProps {
  intervalSeconds?: number;
}

interface ChartPalette {
  streamA: string;
  streamABg: string;
  streamB: string;
  streamBBg: string;
  grid: string;
  text: string;
  textLight: string;
  textStrong: string;
  tooltipBg: string;
}

const terminalFont = {
  family: "'JetBrains Mono', monospace",
  size: 10,
};

const MAX_Y_AXIS_DECIMALS = 6;
const TARGET_Y_AXIS_TICKS = 6;

function readCssVar(variableName: string, fallback: string): string {
  if (typeof window === "undefined") {
    return fallback;
  }

  const value = getComputedStyle(document.documentElement)
    .getPropertyValue(variableName)
    .trim();

  return value || fallback;
}

function getChartPalette(): ChartPalette {
  return {
    streamA: readCssVar("--monitor-accent-a", "#43df8f"),
    streamABg: readCssVar("--monitor-glow-a", "rgba(67, 223, 143, 0.16)"),
    streamB: readCssVar("--monitor-accent-b", "#57b7ff"),
    streamBBg: readCssVar("--monitor-glow-b", "rgba(87, 183, 255, 0.16)"),
    grid: readCssVar("--monitor-border-strong", "#3f5f8f"),
    text: readCssVar("--monitor-text-faint", "#5f7598"),
    textLight: readCssVar("--monitor-text-dim", "#91a8cb"),
    textStrong: readCssVar("--monitor-text-strong", "#f4f8ff"),
    tooltipBg: readCssVar("--monitor-bg-0", "#050812"),
  };
}

function ChartLoader() {
  return <div className="monitor-chart-loader h-full">Loading chart</div>;
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

interface AdaptiveDomainOptions {
  defaultMin: number;
  defaultMax: number;
  minPadding: number;
  minSpan: number;
  paddingRatio: number;
  flatPaddingRatio: number;
  clampMin?: number;
  clampMax?: number;
}

function clampValue(value: number, min?: number, max?: number): number {
  let next = value;
  if (typeof min === "number") {
    next = Math.max(min, next);
  }
  if (typeof max === "number") {
    next = Math.min(max, next);
  }
  return next;
}

function getAdaptiveDomain(values: number[], options: AdaptiveDomainOptions): { min: number; max: number } {
  const finiteValues = values.filter((value) => Number.isFinite(value));
  const fallback = { min: options.defaultMin, max: options.defaultMax };

  if (finiteValues.length === 0) {
    return fallback;
  }

  let nextMin = Math.min(...finiteValues);
  let nextMax = Math.max(...finiteValues);

  if (nextMin === nextMax) {
    if (nextMin === 0) {
      return fallback;
    }

    const halfSpan = Math.max(
      Math.abs(nextMin) * options.flatPaddingRatio,
      options.minPadding,
      options.minSpan / 2,
    );
    nextMin -= halfSpan;
    nextMax += halfSpan;
  } else {
    const padding = Math.max((nextMax - nextMin) * options.paddingRatio, options.minPadding);
    nextMin -= padding;
    nextMax += padding;
  }

  nextMin = clampValue(nextMin, options.clampMin, options.clampMax);
  nextMax = clampValue(nextMax, options.clampMin, options.clampMax);

  if (nextMax - nextMin < options.minSpan) {
    const midpoint = (nextMin + nextMax) / 2;
    nextMin = clampValue(midpoint - options.minSpan / 2, options.clampMin, options.clampMax);
    nextMax = clampValue(midpoint + options.minSpan / 2, options.clampMin, options.clampMax);
  }

  if (!Number.isFinite(nextMin) || !Number.isFinite(nextMax) || nextMin >= nextMax) {
    return fallback;
  }

  return { min: nextMin, max: nextMax };
}

function parseHour(value: string): Date | null {
  const normalized = value.includes("T") ? value : `${value.replace(" ", "T")}Z`;
  const parsed = new Date(normalized);
  if (Number.isNaN(parsed.getTime())) {
    return null;
  }
  return parsed;
}

function formatHourLabel(value: string): string {
  const parsed = parseHour(value);
  if (!parsed) return value;
  return parsed.toLocaleString("en-US", {
    month: "short",
    day: "numeric",
    hour: "2-digit",
  });
}

function getAdaptiveTickDecimals(min: number, max: number): number {
  const span = Math.abs(max - min);
  if (!Number.isFinite(span) || span <= 0) {
    return 0;
  }

  const step = span / TARGET_Y_AXIS_TICKS;
  if (!Number.isFinite(step) || step <= 0) {
    return 0;
  }

  return Math.min(MAX_Y_AXIS_DECIMALS, Math.max(0, Math.ceil(-Math.log10(step))));
}

function normalizeTickValue(value: number): number {
  const normalized = Number.parseFloat(value.toFixed(MAX_Y_AXIS_DECIMALS));
  return Object.is(normalized, -0) ? 0 : normalized;
}

function formatAdaptiveYAxisTick(
  tickValue: number | string,
  min: number,
  max: number,
  suffix: string,
): string {
  const numericValue = typeof tickValue === "number" ? tickValue : Number(tickValue);
  if (!Number.isFinite(numericValue)) {
    return `${tickValue}${suffix}`;
  }

  const decimals = getAdaptiveTickDecimals(min, max);
  const normalized = normalizeTickValue(numericValue);
  const rounded = Number.parseFloat(normalized.toFixed(decimals));
  const text = rounded.toFixed(decimals).replace(/\.?0+$/, "");
  return `${text}${suffix}`;
}

function getStateMessage(state: ChartRenderState): string {
  switch (state) {
    case "loading":
      return "Loading selected window";
    case "stale":
      return "Showing stale window data";
    case "disconnected":
      return "Transport disconnected";
    case "no_data":
      return "No data in selected window";
    default:
      return "";
  }
}

function getStateOverlayMessage(state: ChartRenderState): string {
  switch (state) {
    case "loading":
      return "Refreshing window";
    case "stale":
      return "Stale data: displaying most recent successful fetch";
    case "disconnected":
      return "Disconnected: displaying last known window";
    default:
      return "";
  }
}

function ChartCardShell({
  title,
  body,
  renderState,
}: {
  title: string;
  body: JSX.Element;
  renderState?: ChartRenderState;
}) {
  return (
    <div className="monitor-panel monitor-chart-card" data-render-state={renderState}>
      <div className="monitor-chart-header">
        <h3 className="monitor-chart-title">{title}</h3>
      </div>
      {body}
    </div>
  );
}

function ChartStateEmpty({
  title,
  state,
  heightClass,
}: {
  title: string;
  state: ChartRenderState;
  heightClass: string;
}) {
  return (
    <ChartCardShell
      title={title}
      renderState={state}
      body={
        <div className={`monitor-state-empty monitor-chart-frame ${heightClass}`}>
          {getStateMessage(state)}
        </div>
      }
    />
  );
}

function ChartStateOverlay({ state }: { state: ChartRenderState }) {
  const message = getStateOverlayMessage(state);
  if (!message) return null;

  return <div className={`monitor-state-overlay monitor-state-overlay--${state}`}>{message}</div>;
}

export function UptimeChart24h({
  data,
  streamAName,
  streamBName,
  renderState = "ready",
  windowLabel = "24H",
}: ChartProps) {
  const hasData = data.length > 0;
  const title = `UPTIME ${windowLabel}`;
  const palette = useMemo(() => getChartPalette(), []);

  const chartContent = useMemo(() => {
    if (!hasData) return null;

    const labels = data.map((row) => formatHourLabel(row.hour));
    const uptimeA = data.map((row) => {
      const uptime = toNonNegative(row.stream_a_seconds);
      const downtime = toNonNegative(row.stream_a_downtime_seconds);
      const observed = uptime + downtime;
      return observed > 0 ? clampPercent((uptime / observed) * 100) : 0;
    });
    const uptimeB = data.map((row) => {
      const uptime = toNonNegative(row.stream_b_seconds);
      const downtime = toNonNegative(row.stream_b_downtime_seconds);
      const observed = uptime + downtime;
      return observed > 0 ? clampPercent((uptime / observed) * 100) : 0;
    });
    const uptimeDomain = getAdaptiveDomain([...uptimeA, ...uptimeB], {
      defaultMin: 0,
      defaultMax: 100,
      minPadding: 0.25,
      minSpan: 2,
      paddingRatio: 0.12,
      flatPaddingRatio: 0.03,
      clampMin: 0,
      clampMax: 100,
    });

    return {
      chartData: {
        labels,
        datasets: [
          {
            label: streamAName,
            data: uptimeA,
            backgroundColor: palette.streamA,
            borderColor: palette.streamA,
            borderWidth: 0,
            borderRadius: 0,
            borderSkipped: false,
          },
          {
            label: streamBName,
            data: uptimeB,
            backgroundColor: palette.streamB,
            borderColor: palette.streamB,
            borderWidth: 0,
            borderRadius: 0,
            borderSkipped: false,
          },
        ],
      },
      options: {
        responsive: true,
        maintainAspectRatio: false,
        plugins: {
          legend: {
            position: "top" as const,
            align: "end" as const,
            labels: {
              color: palette.textLight,
              usePointStyle: true,
              pointStyle: "rect" as const,
              padding: 16,
              font: terminalFont,
            },
          },
          tooltip: {
            backgroundColor: palette.tooltipBg,
            titleColor: palette.textStrong,
            bodyColor: palette.textLight,
            borderColor: palette.grid,
            borderWidth: 2,
            cornerRadius: 0,
            padding: 10,
            titleFont: terminalFont,
            bodyFont: terminalFont,
          },
        },
        scales: {
          x: {
            ticks: { color: palette.text, maxTicksLimit: 12, font: terminalFont },
            grid: { color: palette.grid, drawTicks: false },
          },
          y: {
            ticks: {
              color: palette.text,
              callback: (v: number | string) =>
                formatAdaptiveYAxisTick(v, uptimeDomain.min, uptimeDomain.max, "%"),
              font: terminalFont,
            },
            grid: { color: palette.grid, drawTicks: false },
            min: uptimeDomain.min,
            max: uptimeDomain.max,
          },
        },
      },
    };
  }, [data, hasData, palette, streamAName, streamBName]);

  if (!chartContent) {
    return <ChartStateEmpty title={title} state={renderState} heightClass="h-[288px]" />;
  }

  return (
    <ChartCardShell
      title={title}
      renderState={renderState}
      body={
        <>
          <ChartStateOverlay state={renderState} />
          <div className="monitor-chart-frame relative h-[288px] w-full">
            <Suspense fallback={<ChartLoader />}>
              <Bar data={chartContent.chartData} options={chartContent.options} />
            </Suspense>
          </div>
        </>
      }
    />
  );
}

export function UptimeChart28d(props: ChartProps) {
  return <UptimeChart24h {...props} windowLabel={props.windowLabel ?? "28D"} />;
}

export function RateChart({
  data,
  streamAName,
  streamBName,
  renderState = "ready",
  windowLabel = "24H",
  intervalSeconds = 3600,
}: RateChartProps) {
  const hasData = data.length > 0;
  const title = `MESSAGE RATE ${windowLabel}`;
  const palette = useMemo(() => getChartPalette(), []);

  const chartContent = useMemo(() => {
    if (!hasData) return null;

    const safeIntervalSeconds = Math.max(1, Math.floor(intervalSeconds));
    const labels = data.map((row) => formatHourLabel(row.hour));
    const rateA = data.map((row) => {
      const messages = toNonNegative(row.stream_a_messages);
      const observed =
        toNonNegative(row.stream_a_seconds) +
        toNonNegative(row.stream_a_downtime_seconds);
      const denominator = observed > 0 ? observed : safeIntervalSeconds;
      return messages / denominator;
    });
    const rateB = data.map((row) => {
      const messages = toNonNegative(row.stream_b_messages);
      const observed =
        toNonNegative(row.stream_b_seconds) +
        toNonNegative(row.stream_b_downtime_seconds);
      const denominator = observed > 0 ? observed : safeIntervalSeconds;
      return messages / denominator;
    });
    const rateDomain = getAdaptiveDomain([...rateA, ...rateB], {
      defaultMin: 0,
      defaultMax: 1,
      minPadding: 0.05,
      minSpan: 0.2,
      paddingRatio: 0.12,
      flatPaddingRatio: 0.2,
      clampMin: 0,
    });

    return {
      chartData: {
        labels,
        datasets: [
          {
            label: streamAName,
            data: rateA,
            borderColor: palette.streamA,
            backgroundColor: "transparent",
            tension: 0.12,
            borderWidth: 2,
            pointRadius: 2,
            pointHoverRadius: 4,
            pointHoverBorderWidth: 2,
          },
          {
            label: streamBName,
            data: rateB,
            borderColor: palette.streamB,
            backgroundColor: "transparent",
            tension: 0.12,
            borderWidth: 2,
            pointRadius: 2,
            pointHoverRadius: 4,
            pointHoverBorderWidth: 2,
          },
        ],
      },
      options: {
        responsive: true,
        maintainAspectRatio: false,
        plugins: {
          legend: {
            position: "top" as const,
            align: "end" as const,
            labels: {
              color: palette.textLight,
              usePointStyle: true,
              pointStyle: "rect" as const,
              padding: 16,
              font: terminalFont,
            },
          },
          tooltip: {
            backgroundColor: palette.tooltipBg,
            titleColor: palette.textStrong,
            bodyColor: palette.textLight,
            borderColor: palette.grid,
            borderWidth: 2,
            cornerRadius: 0,
            padding: 10,
            titleFont: terminalFont,
            bodyFont: terminalFont,
          },
        },
        scales: {
          x: {
            ticks: { color: palette.text, maxTicksLimit: 12, font: terminalFont },
            grid: { color: palette.grid, drawTicks: false },
          },
          y: {
            ticks: {
              color: palette.text,
              callback: (v: number | string) =>
                formatAdaptiveYAxisTick(v, rateDomain.min, rateDomain.max, "/s"),
              font: terminalFont,
            },
            grid: { color: palette.grid, drawTicks: false },
            min: rateDomain.min,
            max: rateDomain.max,
          },
        },
      },
    };
  }, [data, hasData, intervalSeconds, palette, streamAName, streamBName]);

  if (!chartContent) {
    return <ChartStateEmpty title={title} state={renderState} heightClass="h-[236px]" />;
  }

  return (
    <ChartCardShell
      title={title}
      renderState={renderState}
      body={
        <>
          <ChartStateOverlay state={renderState} />
          <div className="monitor-chart-frame relative h-[236px] w-full">
            <Suspense fallback={<ChartLoader />}>
              <Line data={chartContent.chartData} options={chartContent.options} />
            </Suspense>
          </div>
        </>
      }
    />
  );
}
