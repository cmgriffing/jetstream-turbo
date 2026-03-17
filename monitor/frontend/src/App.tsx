import { useState, useCallback, useEffect, useMemo } from "react";
import { Header } from "@/components/Header";
import { StreamCard } from "@/components/StreamCard";
import { MetricsTable } from "@/components/MetricsTable";
import { StatusIndicator } from "@/components/StatusIndicator";
import { ConnectionBanner } from "@/components/ConnectionBanner";
import { UptimeChart24h, RateChart } from "@/components/Charts";
import { DeltaCard } from "@/components/DeltaCard";
import {
  StreamStats,
  useWebSocket,
  useUptimeHistory,
  ConnectionStatus,
} from "@/hooks/useStream";

type HistoryRenderState =
  | "loading"
  | "no_data"
  | "stale"
  | "disconnected"
  | "ready";

type PillTone = "good" | "info" | "warn" | "bad" | "neutral";

const HISTORY_WINDOWS = [
  { label: "24H", hours: 24 },
  { label: "7D", hours: 24 * 7 },
  { label: "28D", hours: 24 * 28 },
] as const;

function formatWindowLabel(hours: number): string {
  if (hours % 24 === 0) {
    const days = hours / 24;
    return days === 1 ? "24H" : `${days}D`;
  }
  return `${hours}H`;
}

function getHistoryStateLabel(state: HistoryRenderState): string {
  switch (state) {
    case "loading":
      return "Refreshing Window";
    case "stale":
      return "Stale Snapshot";
    case "disconnected":
      return "Transport Offline";
    case "no_data":
      return "No Analytics Yet";
    default:
      return "Window Healthy";
  }
}

function getHistoryStateTone(state: HistoryRenderState): PillTone {
  switch (state) {
    case "ready":
      return "good";
    case "loading":
      return "info";
    case "stale":
      return "warn";
    case "disconnected":
      return "bad";
    case "no_data":
      return "neutral";
    default:
      return "neutral";
  }
}

function getTransportState(status: ConnectionStatus): { label: string; tone: PillTone } {
  if (status === "connected") {
    return { label: "Transport Live", tone: "good" };
  }
  if (status === "connecting") {
    return { label: "Transport Connecting", tone: "info" };
  }
  return { label: "Transport Down", tone: "bad" };
}

function formatRefreshLabel(lastUpdatedAt: number | null): string {
  if (!lastUpdatedAt) {
    return "Last refresh --";
  }
  const formatted = new Date(lastUpdatedAt).toLocaleTimeString(undefined, {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
  return `Last refresh ${formatted}`;
}

function App() {
  const [stats, setStats] = useState<StreamStats>({});
  const [connectionStatus, setConnectionStatus] =
    useState<ConnectionStatus>("connecting");
  const [historyHours, setHistoryHours] = useState(24);
  const [connectedAt, setConnectedAt] = useState<number | null>(null);

  const handleMessage = useCallback((newStats: StreamStats) => {
    setStats(newStats);
  }, []);

  const wsProtocol = window.location.protocol === "https:" ? "wss:" : "ws:";
  const wsUrl = `${wsProtocol}//${window.location.host}/ws`;

  useWebSocket(wsUrl, handleMessage, setConnectionStatus);

  const {
    data: hourlyData,
    spanSeconds,
    requestedWindowSeconds,
    intervalSeconds,
    loading: historyLoading,
    isStale: historyStale,
    lastUpdatedAt,
  } = useUptimeHistory(historyHours);

  const currentDiff = (stats.stream_a || 0) - (stats.stream_b || 0);
  const transportConnected = connectionStatus === "connected";
  const streamAName = stats.stream_a_name || "Stream A";
  const streamBName = stats.stream_b_name || "Stream B";
  const windowLabel = formatWindowLabel(historyHours);

  useEffect(() => {
    if (transportConnected) {
      setConnectedAt((prev) => prev ?? Date.now());
      return;
    }
    setConnectedAt(null);
  }, [transportConnected]);

  const historyRenderState = useMemo<HistoryRenderState>(() => {
    if (connectionStatus === "disconnected") return "disconnected";
    if (historyLoading && hourlyData.length === 0) return "loading";
    if (historyStale) return "stale";
    if (hourlyData.length === 0) return "no_data";
    return "ready";
  }, [connectionStatus, historyLoading, hourlyData.length, historyStale]);

  const transportState = getTransportState(connectionStatus);
  const historyStateLabel = getHistoryStateLabel(historyRenderState);
  const historyStateTone = getHistoryStateTone(historyRenderState);

  return (
    <div className="monitor-app">
      <ConnectionBanner status={connectionStatus} />

      <div className="monitor-shell monitor-shell-rail">
        <Header
          title="Jetstream Signal Monitor"
          subtitle="Operational comparison across live throughput, reliability, and historical behavior"
        />

        <main className="monitor-main monitor-main-grid" aria-label="Monitor data sections">
          <section
            className="monitor-section monitor-section--framed monitor-section--priority monitor-section--health"
            aria-label="Current health and status"
          >
            <div className="monitor-section-heading">
              <div className="monitor-section-copyblock">
                <p className="monitor-eyebrow">Current Health</p>
                <h2 className="monitor-section-title">Primary live status and stream dominance</h2>
                <p className="monitor-section-copy">
                  Connection health and headline message position are surfaced first for rapid operator
                  scanning. Stream uptime here reflects all-time live aggregates.
                </p>
              </div>
              <div className="monitor-heading-meta">
                <span className={`monitor-pill monitor-pill--${transportState.tone}`}>
                  {transportState.label}
                </span>
                <span className={`monitor-pill monitor-pill--${historyStateTone}`}>
                  {historyStateLabel}
                </span>
              </div>
            </div>

            <div className="monitor-health-row">
              <DeltaCard
                delta={currentDiff}
                streamAName={stats.stream_a_name || "STREAM_A"}
                streamBName={stats.stream_b_name || "STREAM_B"}
              />

              <div className="monitor-stream-grid">
                <StreamCard
                  streamId="a"
                  name={stats.stream_a_name || "STREAM_A"}
                  count={stats.stream_a || 0}
                  countingStartedAt={stats.counting_started_at}
                  rate={stats.rate_a || 0}
                  streak={stats.current_streak_a}
                  uptimeAllTime={stats.uptime_a_all_time}
                  connected={stats.connected_a || false}
                />
                <StreamCard
                  streamId="b"
                  name={stats.stream_b_name || "STREAM_B"}
                  count={stats.stream_b || 0}
                  countingStartedAt={stats.counting_started_at}
                  rate={stats.rate_b || 0}
                  streak={stats.current_streak_b}
                  uptimeAllTime={stats.uptime_b_all_time}
                  connected={stats.connected_b || false}
                />
              </div>
            </div>
          </section>

          <section
            className="monitor-section monitor-section--framed monitor-section--throughput"
            aria-label="Comparative throughput"
          >
            <div className="monitor-section-heading">
              <div className="monitor-section-copyblock">
                <p className="monitor-eyebrow">Comparative Throughput</p>
                <h2 className="monitor-section-title">Message-rate divergence across selected windows</h2>
                <p className="monitor-section-copy">
                  Window controls update throughput and analytics together so trend interpretation stays
                  consistent.
                </p>
              </div>
            </div>

            <div className="monitor-window-controls" role="group" aria-label="Analytics window">
              <p className="monitor-window-label">Analytics window</p>
              <div className="monitor-window-list">
                {HISTORY_WINDOWS.map((window) => {
                  const selected = window.hours === historyHours;
                  return (
                    <button
                      key={window.hours}
                      type="button"
                      onClick={() => setHistoryHours(window.hours)}
                      className={`monitor-window-button ${selected ? "monitor-window-button--active" : ""}`}
                      aria-pressed={selected}
                    >
                      {window.label}
                    </button>
                  );
                })}
              </div>
              <div className="monitor-last-refresh">{formatRefreshLabel(lastUpdatedAt)}</div>
            </div>

            <RateChart
              data={hourlyData}
              streamAName={streamAName}
              streamBName={streamBName}
              renderState={historyRenderState}
              intervalSeconds={intervalSeconds}
              windowLabel={windowLabel}
            />
          </section>

          <section
            className="monitor-section monitor-section--framed monitor-section--reliability"
            aria-label="Reliability trends"
          >
            <div className="monitor-section-heading">
              <div className="monitor-section-copyblock">
                <p className="monitor-eyebrow">Reliability Trends</p>
                <h2 className="monitor-section-title">Observed uptime stability by interval</h2>
                <p className="monitor-section-copy">
                  Uptime percentages are computed from observed up and down seconds in each interval.
                </p>
              </div>
            </div>

            <UptimeChart24h
              data={hourlyData}
              streamAName={streamAName}
              streamBName={streamBName}
              renderState={historyRenderState}
              windowLabel={windowLabel}
            />
          </section>

          <section
            className="monitor-section monitor-section--framed monitor-section--history"
            aria-label="Historical analytics table"
          >
            <div className="monitor-section-heading">
              <div className="monitor-section-copyblock">
                <p className="monitor-eyebrow">Historical Analytics</p>
                <h2 className="monitor-section-title">Operational summary for the selected window</h2>
                <p className="monitor-section-copy">
                  Selected-window reliability, coverage, disconnect, and recovery signals are presented in a
                  single comparative surface.
                </p>
              </div>
            </div>

            <MetricsTable
              title={`Last ${windowLabel}`}
              icon="DATA"
              data={hourlyData}
              spanSeconds={spanSeconds}
              requestedWindowSeconds={requestedWindowSeconds}
              intervalSeconds={intervalSeconds}
              renderState={historyRenderState}
              streamAName={streamAName}
              streamBName={streamBName}
              windowLabel={windowLabel}
            />
          </section>
        </main>

        <StatusIndicator
          connected={transportConnected}
          connectedAt={connectedAt}
        />
      </div>
    </div>
  );
}

export default App;
