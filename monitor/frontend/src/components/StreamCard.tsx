interface StreamCardProps {
  streamId: "a" | "b";
  name: string;
  count: number;
  rate: number;
  streak?: number;
  uptime?: number;
  uptimeAllTime?: number;
  connected: boolean;
}

import { Zap, Info } from "lucide-react"
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Badge } from "@/components/ui/badge"
import { cn } from "@/lib/utils"

function formatDuration(ms: number): string {
  const secs = Math.floor(ms / 1000);
  const hrs = Math.floor(secs / 3600);
  const mins = Math.floor((secs % 3600) / 60);
  if (hrs > 0) return `${hrs}h ${mins}m`;
  if (mins > 0) return `${mins}m ${secs % 60}s`;
  return `${secs}s`;
}

export function StreamCard({
  streamId,
  name,
  count,
  rate,
  streak,
  uptimeAllTime,
  connected,
}: StreamCardProps) {

  return (
    <Card className={cn(
      "flex-1 min-w-[280px] max-w-[400px]",
      streamId === "a" && "border-l-4 border-l-green-600 dark:border-l-green-500",
      streamId === "b" && "border-l-4 border-l-blue-600 dark:border-l-blue-500"
    )}>
      <CardHeader className="flex flex-row items-center justify-between pb-2">
        <CardTitle className={cn(
          "text-sm font-semibold uppercase tracking-wider",
          streamId === "a" && "text-green-600 dark:text-green-500",
          streamId === "b" && "text-blue-600 dark:text-blue-500"
        )}>
          {name}
        </CardTitle>
        <Badge variant={connected ? "default" : "destructive"} className={cn(
          "text-xs uppercase tracking-wider",
          connected && "bg-green-600 hover:bg-green-600 dark:bg-green-500 dark:hover:bg-green-500"
        )}>
          {connected ? "Connected" : "Disconnected"}
        </Badge>
      </CardHeader>
      <CardContent>
        <div className="mb-2">
          <div className="text-5xl font-bold tabular-nums">
            {count.toLocaleString()}
          </div>
          <div className="text-sm text-muted-foreground uppercase tracking-wider flex items-center gap-2 mt-1">
            <Zap className="w-4 h-4" />
            messages
          </div>
        </div>

        <div className="grid grid-cols-3 gap-4 pt-4 border-t">
          <div>
            <span className="text-xs text-muted-foreground uppercase tracking-wider block mb-1 flex items-center gap-1">
              Rate
              <div className="group relative">
                <Info className="w-3 h-3" />
                <div className="absolute bottom-full left-1/2 -translate-x-1/2 mb-2 w-48 p-2 bg-popover text-popover-foreground text-xs rounded-lg opacity-0 group-hover:opacity-100 transition-opacity pointer-events-none z-10 shadow-md">
                  Average messages/second while connected since server started
                </div>
              </div>
            </span>
            <span className="text-lg font-semibold tabular-nums">
              {rate.toFixed(0)}/s
            </span>
          </div>
          <div>
            <span className="text-xs text-muted-foreground uppercase tracking-wider block mb-1">
              Streak
            </span>
            <span className="text-lg font-semibold tabular-nums">
              {streak ? formatDuration(streak * 1000) : "-"}
            </span>
          </div>
          <div>
            <span className="text-xs text-muted-foreground uppercase tracking-wider block mb-1">
              Uptime (All Time)
            </span>
            <span className="text-lg font-semibold tabular-nums">
              {uptimeAllTime !== undefined ? `${uptimeAllTime.toFixed(1)}%` : "-"}
            </span>
          </div>
        </div>
      </CardContent>
    </Card>
  );
}
