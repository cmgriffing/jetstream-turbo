import { useState, useEffect } from "react";
import { Wifi, WifiOff } from "lucide-react";
import { cn } from "@/lib/utils";

interface StatusIndicatorProps {
  connected: boolean;
  connectedAt?: number | null;
}

function formatDuration(ms: number): string {
  const secs = Math.floor(ms / 1000);
  const hrs = Math.floor(secs / 3600);
  const mins = Math.floor((secs % 3600) / 60);
  if (hrs > 0) return `${hrs}h ${mins}m`;
  if (mins > 0) return `${mins}m ${secs % 60}s`;
  return `${secs}s`;
}

export function StatusIndicator({ connected, connectedAt }: StatusIndicatorProps) {
  const [duration, setDuration] = useState("");

  useEffect(() => {
    if (!connected || !connectedAt) {
      setDuration("");
      return;
    }

    const updateDuration = () => {
      setDuration(formatDuration(Date.now() - connectedAt));
    };

    updateDuration();
    const interval = setInterval(updateDuration, 1000);
    return () => clearInterval(interval);
  }, [connected, connectedAt]);

  return (
    <div className="monitor-status-indicator" aria-live="polite">
      <div
        className={cn(
          "monitor-status-pill",
          connected ? "monitor-status-pill--online" : "monitor-status-pill--offline",
        )}
      >
        {connected ? <Wifi className="h-3 w-3" aria-hidden="true" /> : <WifiOff className="h-3 w-3" aria-hidden="true" />}
        {connected ? "Online" : "Offline"}
      </div>
      {connected && duration && (
        <span className="monitor-status-duration">Session {duration}</span>
      )}
    </div>
  );
}
