import { memo } from "react"
import { Zap, Info, Circle } from "lucide-react"
import { cn } from "@/lib/utils"

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
  const streamColor = streamId === "a" ? "#3fb950" : "#58a6ff";

  return (
    <div className={cn(
      "flex-1 min-w-[320px] max-w-[420px] bg-[#0f0f0f] border border-[#1a1a1a] rounded-sm p-5 transition-all duration-200 hover:border-[#252525] hover:bg-[#111111]",
      streamId === "a" && "border-l-[2px] border-l-[#3fb950]",
      streamId === "b" && "border-l-[2px] border-l-[#58a6ff]"
    )}>
      <div className="flex flex-row items-center justify-between mb-4">
        <div className="flex items-center gap-2">
          <span className="text-[10px] font-mono text-[#525252]">$</span>
          <span className="text-sm font-semibold tracking-wide" style={{ color: streamColor }}>
            {name}
          </span>
        </div>
        <div className="flex items-center gap-1.5">
          <Circle 
            className="w-2 h-2 fill-current" 
            style={{ color: connected ? streamColor : '#f85149' }}
          />
          <span className="text-[10px] font-mono tracking-wider" style={{ color: connected ? streamColor : '#f85149' }}>
            {connected ? 'CONNECTED' : 'DISCONNECTED'}
          </span>
        </div>
      </div>

      <div className="mb-4">
        <div className="text-4xl font-mono font-medium tracking-tight text-[#e5e5e5]">
          {count.toLocaleString()}
        </div>
        <div className="text-[10px] text-[#525252] font-mono tracking-[0.15em] flex items-center gap-1.5 mt-1">
          <Zap className="w-3 h-3" />
          TOTAL_MESSAGES
        </div>
        <div className="text-[10px] text-[#525252] font-mono tracking-[0.1em] mt-1">
          SINCE {formatCountingStartedAt(countingStartedAt)}
        </div>
      </div>

      <div className="grid grid-cols-4 gap-4 pt-3 border-t border-[#1a1a1a]">
        <div>
          <span className="text-[10px] text-[#525252] font-mono tracking-[0.1em] block mb-1 flex items-center gap-1">
            RATE
            <button
                type="button"
                className="group relative cursor-pointer"
                aria-label="More info about rate"
              >
                <Info className="w-2.5 h-2.5" />
              <div className="absolute bottom-full left-1/2 -translate-x-1/2 mb-2 w-48 p-2 bg-[#1a1a1a] text-[#8a8a8a] text-[10px] font-mono rounded opacity-0 group-hover:opacity-100 transition-opacity pointer-events-none z-10 border border-[#252525]">
                Messages/second over the last 10 seconds
              </div>
            </button>
          </span>
          <span className="text-base font-mono text-[#e5e5e5]">
            {rate.toFixed(0)}<span className="text-[#525252] text-xs">/s</span>
          </span>
        </div>
        <div>
          <span className="text-[10px] text-[#525252] font-mono tracking-[0.1em] block mb-1 flex items-center gap-1">
            LATENCY
            <button
                type="button"
                className="group relative cursor-pointer"
                aria-label="More info about delivery latency"
              >
                <Info className="w-2.5 h-2.5" />
              <div className="absolute bottom-full left-1/2 -translate-x-1/2 mb-2 w-48 p-2 bg-[#1a1a1a] text-[#8a8a8a] text-[10px] font-mono rounded opacity-0 group-hover:opacity-100 transition-opacity pointer-events-none z-10 border border-[#252525]">
                Message delivery latency (avg over last 10s)
              </div>
            </button>
          </span>
          <span className="text-base font-mono text-[#e5e5e5]">
            {deliveryLatencyMs !== undefined && deliveryLatencyMs > 0 ? (
              <>{deliveryLatencyMs < 1000 ? `${deliveryLatencyMs.toFixed(0)}` : `${(deliveryLatencyMs / 1000).toFixed(1)}s`}<span className="text-[#525252] text-xs">{deliveryLatencyMs < 1000 ? 'ms' : ''}</span></>
            ) : <span className="text-[#525252]">--</span>}
          </span>
        </div>
        <div>
          <span className="text-[10px] text-[#525252] font-mono tracking-[0.1em] block mb-1">
            STREAK
          </span>
          <span className="text-base font-mono text-[#e5e5e5]">
            {streak ? formatDuration(streak * 1000) : <span className="text-[#525252]">--</span>}
          </span>
        </div>
        <div>
          <span className="text-[10px] text-[#525252] font-mono tracking-[0.1em] block mb-1">
            UPTIME
          </span>
          <span className="text-base font-mono text-[#e5e5e5]">
            {uptimeAllTime !== undefined ? (
              <>{uptimeAllTime.toFixed(1)}<span className="text-[#525252] text-xs">%</span></>
            ) : <span className="text-[#525252]">--</span>}
          </span>
        </div>
      </div>
    </div>
  );
})
