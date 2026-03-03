import { useState, useCallback } from "react";
import { Header } from "./components/Header";
import { StreamCard } from "./components/StreamCard";
import { DeltaCard } from "./components/DeltaCard";
import { MetricsTable } from "./components/MetricsTable";
import { UptimeChart24h, UptimeChart28d, RateChart } from "./components/Charts";
import { StatusIndicator } from "./components/StatusIndicator";
import { useWebSocket, useUptimeHistory, StreamStats } from "./hooks/useStream";

function App() {
  const [streamAName, setStreamAName] = useState("Stream A");
  const [streamBName, setStreamBName] = useState("Stream B");
  const [countA, setCountA] = useState(0);
  const [countB, setCountB] = useState(0);
  const [rateA, setRateA] = useState(0);
  const [rateB, setRateB] = useState(0);
  const [streakA, setStreakA] = useState<number | undefined>();
  const [streakB, setStreakB] = useState<number | undefined>();
  const [uptimeA, setUptimeA] = useState<number | undefined>();
  const [uptimeB, setUptimeB] = useState<number | undefined>();
  const [uptimeAAllTime, setUptimeAAllTime] = useState<number | undefined>();
  const [uptimeBAllTime, setUptimeBAllTime] = useState<number | undefined>();
  const [connectedA, setConnectedA] = useState(false);
  const [connectedB, setConnectedB] = useState(false);
  const [connected, setConnected] = useState(false);
  const [connectedAt, setConnectedAt] = useState<number | null>(null);

  const { data: uptimeData24h, spanSeconds: span24h } = useUptimeHistory(24);
  const { data: uptimeData28d, spanSeconds: span28d } = useUptimeHistory(672);

  const handleMessage = useCallback(
    (stats: StreamStats) => {
      if (stats.stream_a_name) {
        setStreamAName(stats.stream_a_name);
        setStreamBName(stats.stream_b_name || "Stream B");
      }

      if (stats.stream_a !== undefined) {
        setCountA(stats.stream_a);
        setRateA(stats.rate_a || 0);
        setStreakA(stats.current_streak_a);
        setUptimeA(stats.uptime_a);
        setUptimeAAllTime(stats.uptime_a_all_time);
        setConnectedA(stats.connected_a ?? false);
      }

      if (stats.stream_b !== undefined) {
        setCountB(stats.stream_b);
        setRateB(stats.rate_b || 0);
        setStreakB(stats.current_streak_b);
        setUptimeB(stats.uptime_b);
        setUptimeBAllTime(stats.uptime_b_all_time);
        setConnectedB(stats.connected_b ?? false);
      }

      setConnected(connectedA || connectedB);
      if ((connectedA || connectedB) && !connectedAt) {
        setConnectedAt(Date.now());
      }
    },
    [connectedA, connectedB, connectedAt],
  );

  const wsUrl =
    typeof window !== "undefined"
      ? `${window.location.origin.replace("http", "ws")}/ws`
      : "";

  useWebSocket(wsUrl, handleMessage);

  const delta = countA - countB;

  return (
    <>
      <Header />

      <div className="live-section max-w-6xl mx-auto mb-16 px-4">
        <div className="section-label text-[0.6875rem] text-[#525252] uppercase tracking-[0.2em] mb-8 text-center font-medium">
          Live Counters
        </div>
        <div className="live-counters flex flex-wrap md:flex-nowrap items-stretch gap-6 justify-center">
          <StreamCard
            streamId="a"
            name={streamAName}
            count={countA}
            rate={rateA}
            streak={streakA}
            uptime={uptimeA}
            uptimeAllTime={uptimeAAllTime}
            connected={connectedA}
          />

          <DeltaCard delta={delta} />

          <StreamCard
            streamId="b"
            name={streamBName}
            count={countB}
            rate={rateB}
            streak={streakB}
            uptime={uptimeB}
            uptimeAllTime={uptimeBAllTime}
            connected={connectedB}
          />
        </div>
      </div>

      <div className="max-w-6xl mx-auto px-4 overflow-x-hidden">
        <div className="section-divider flex items-center gap-10 mb-12">
          <div className="flex-1 h-px bg-gradient-to-r from-transparent via-[#1f1f1f] to-transparent" />
          <div className="section-title text-[0.8125rem] text-[#525252] uppercase tracking-[0.2em] font-medium">
            Metrics Dashboard
          </div>
          <div className="flex-1 h-px bg-gradient-to-r from-transparent via-[#1f1f1f] to-transparent" />
        </div>

        <div className="metrics-section mb-12">
          <div className="metrics-grid grid grid-cols-2 gap-6">
            <MetricsTable
              title="24-Hour Summary"
              icon="📊"
              data={uptimeData24h}
              spanSeconds={span24h}
              streamAName={streamAName}
              streamBName={streamBName}
            />
            <MetricsTable
              title="28-Day Summary"
              icon="📅"
              data={uptimeData28d}
              spanSeconds={span28d}
              streamAName={streamAName}
              streamBName={streamBName}
            />
          </div>
        </div>

        <div className="charts-section mb-8">
          <UptimeChart24h
            data={uptimeData24h}
            streamAName={streamAName}
            streamBName={streamBName}
          />
          <UptimeChart28d
            data={uptimeData28d}
            streamAName={streamAName}
            streamBName={streamBName}
          />
          <RateChart
            data={uptimeData24h}
            streamAName={streamAName}
            streamBName={streamBName}
          />
        </div>
      </div>

      <StatusIndicator connected={connected} connectedAt={connectedAt} />
    </>
  );
}

export default App;
