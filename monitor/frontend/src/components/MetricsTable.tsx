import { HourlyUptime } from '../hooks/useStream'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"

interface MetricsTableProps {
  title: string
  icon: string
  data: HourlyUptime[]
  spanSeconds: number
  streamAName: string
  streamBName: string
}

function formatDurationLong(seconds: number): string {
  const hrs = Math.floor(seconds / 3600)
  const mins = Math.floor((seconds % 3600) / 60)
  const secs = Math.floor(seconds % 60)
  if (hrs > 0) return `${hrs}h ${mins}m ${secs}s`
  if (mins > 0) return `${mins}m ${secs}s`
  return `${secs}s`
}

function getUptimeClass(percentage: number): string {
  if (percentage >= 99) return 'text-green-600 dark:text-green-500'
  if (percentage >= 95) return 'text-yellow-600 dark:text-yellow-500'
  return 'text-red-600 dark:text-red-500'
}

function calculateStats(data: HourlyUptime[], spanSeconds: number) {
  if (data.length === 0 && spanSeconds === 0) return null

  let totalA = 0
  let totalB = 0
  let disconnectsA = 0
  let disconnectsB = 0
  let messagesA = 0
  let messagesB = 0

  data.forEach((d) => {
    totalA += d.stream_a_seconds || 0
    totalB += d.stream_b_seconds || 0
    disconnectsA += d.stream_a_disconnects || 0
    disconnectsB += d.stream_b_disconnects || 0
    messagesA += d.stream_a_messages || 0
    messagesB += d.stream_b_messages || 0
  })

  const actualSpanSeconds = spanSeconds > 0 ? spanSeconds : (data.length * 3600)

  const uptimeA = (totalA / actualSpanSeconds) * 100
  const uptimeB = (totalB / actualSpanSeconds) * 100

  return {
    uptimeA,
    uptimeB,
    totalA,
    totalB,
    disconnectsA,
    disconnectsB,
    messagesA,
    messagesB,
    rateA: messagesA > 0 && totalA > 0 ? messagesA / totalA : 0,
    rateB: messagesB > 0 && totalB > 0 ? messagesB / totalB : 0,
  }
}

export function MetricsTable({ title, icon, data, spanSeconds, streamAName, streamBName }: MetricsTableProps) {
  const stats = calculateStats(data, spanSeconds)
  const is28d = data.length > 24

  if (!stats) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="text-sm font-semibold flex items-center gap-2">
            {icon} {title}
          </CardTitle>
        </CardHeader>
        <CardContent>
          <p className="text-muted-foreground text-sm">No data available</p>
        </CardContent>
      </Card>
    )
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-sm font-semibold flex items-center gap-2">
          {icon} {title}
        </CardTitle>
      </CardHeader>
      <CardContent>
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead className="w-2/5">Metric</TableHead>
              <TableHead className="text-right w-3/10">{streamAName}</TableHead>
              <TableHead className="text-right w-3/10">{streamBName}</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            <TableRow>
              <TableCell className="text-muted-foreground">Uptime</TableCell>
              <TableCell className={`text-right font-semibold tabular-nums ${getUptimeClass(stats.uptimeA)}`}>
                {stats.uptimeA.toFixed(2)}%
              </TableCell>
              <TableCell className={`text-right font-semibold tabular-nums ${getUptimeClass(stats.uptimeB)}`}>
                {stats.uptimeB.toFixed(2)}%
              </TableCell>
            </TableRow>
            <TableRow>
              <TableCell className="text-muted-foreground">Total Up Time</TableCell>
              <TableCell className="text-right font-semibold tabular-nums">
                {formatDurationLong(stats.totalA)}
              </TableCell>
              <TableCell className="text-right font-semibold tabular-nums">
                {formatDurationLong(stats.totalB)}
              </TableCell>
            </TableRow>
            {!is28d && (
              <TableRow>
                <TableCell className="text-muted-foreground">Messages/sec</TableCell>
                <TableCell className="text-right font-semibold tabular-nums">
                  {stats.rateA.toFixed(1)}/s
                </TableCell>
                <TableCell className="text-right font-semibold tabular-nums">
                  {stats.rateB.toFixed(1)}/s
                </TableCell>
              </TableRow>
            )}
            <TableRow>
              <TableCell className="text-muted-foreground">Disconnects</TableCell>
              <TableCell className="text-right font-semibold tabular-nums">
                {stats.disconnectsA}
              </TableCell>
              <TableCell className="text-right font-semibold tabular-nums">
                {stats.disconnectsB}
              </TableCell>
            </TableRow>
            {!is28d && (
              <TableRow>
                <TableCell className="text-muted-foreground">Total Messages</TableCell>
                <TableCell className="text-right font-semibold tabular-nums">
                  {stats.messagesA.toLocaleString()}
                </TableCell>
                <TableCell className="text-right font-semibold tabular-nums">
                  {stats.messagesB.toLocaleString()}
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </CardContent>
    </Card>
  )
}
