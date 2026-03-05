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

function getUptimeColor(percentage: number): string {
  if (percentage >= 99) return '#3fb950'
  if (percentage >= 95) return '#d29922'
  return '#f85149'
}

function calculateStats(data: HourlyUptime[], spanSeconds: number) {
  if (data.length === 0 && spanSeconds === 0) return null

  let totalA = 0
  let totalB = 0
  let downtimeA = 0
  let downtimeB = 0
  let disconnectsA = 0
  let disconnectsB = 0
  let messagesA = 0
  let messagesB = 0
  let deliveryLatencySumA = 0
  let deliveryLatencySumB = 0
  let deliveryLatencyCountA = 0
  let deliveryLatencyCountB = 0
  let mttrSumA = 0
  let mttrSumB = 0
  let mttrCountA = 0
  let mttrCountB = 0

  data.forEach((d) => {
    totalA += d.stream_a_seconds || 0
    totalB += d.stream_b_seconds || 0
    downtimeA += d.stream_a_downtime_seconds || 0
    downtimeB += d.stream_b_downtime_seconds || 0
    disconnectsA += d.stream_a_disconnects || 0
    disconnectsB += d.stream_b_disconnects || 0
    messagesA += d.stream_a_messages || 0
    messagesB += d.stream_b_messages || 0
    if (d.stream_a_delivery_latency_ms > 0) {
      deliveryLatencySumA += d.stream_a_delivery_latency_ms
      deliveryLatencyCountA++
    }
    if (d.stream_b_delivery_latency_ms > 0) {
      deliveryLatencySumB += d.stream_b_delivery_latency_ms
      deliveryLatencyCountB++
    }
    if (d.stream_a_mttr_ms > 0) {
      mttrSumA += d.stream_a_mttr_ms
      mttrCountA++
    }
    if (d.stream_b_mttr_ms > 0) {
      mttrSumB += d.stream_b_mttr_ms
      mttrCountB++
    }
  })

  const actualSpanSeconds = spanSeconds > 0 ? spanSeconds : (data.length * 3600)

  const uptimeA = actualSpanSeconds > 0 
    ? 100.0 - ((downtimeA / actualSpanSeconds) * 100.0)
    : 0.0
  const uptimeB = actualSpanSeconds > 0 
    ? 100.0 - ((downtimeB / actualSpanSeconds) * 100.0)
    : 0.0

  return {
    uptimeA,
    uptimeB,
    totalA,
    totalB,
    downtimeA,
    downtimeB,
    disconnectsA,
    disconnectsB,
    messagesA,
    messagesB,
    rateA: messagesA > 0 && totalA > 0 ? messagesA / totalA : 0,
    rateB: messagesB > 0 && totalB > 0 ? messagesB / totalB : 0,
    deliveryLatencyA: deliveryLatencyCountA > 0 ? deliveryLatencySumA / deliveryLatencyCountA : 0,
    deliveryLatencyB: deliveryLatencyCountB > 0 ? deliveryLatencySumB / deliveryLatencyCountB : 0,
    mttrA: mttrCountA > 0 ? mttrSumA / mttrCountA : 0,
    mttrB: mttrCountB > 0 ? mttrSumB / mttrCountB : 0,
  }
}

export function MetricsTable({ data, spanSeconds, streamAName, streamBName }: MetricsTableProps) {
  const stats = calculateStats(data, spanSeconds)
  const is28d = data.length > 24

  if (!stats) {
    return (
      <Card className="bg-[#0f0f0f] border-[#1a1a1a]">
        <CardHeader>
          <CardTitle className="text-xs font-semibold text-[#8a8a8a] tracking-wider flex items-center gap-2">
            <span className="text-[10px] font-mono text-[#525252]">//</span>
            LAST_24H
          </CardTitle>
        </CardHeader>
        <CardContent>
          <p className="text-[#525252] text-xs font-mono">NO_DATA</p>
        </CardContent>
      </Card>
    )
  }

  return (
    <Card className="bg-[#0f0f0f] border border-[#1a1a1a] rounded-sm transition-all duration-200 hover:border-[#252525]">
      <CardHeader>
        <CardTitle className="text-xs font-semibold text-[#8a8a8a] tracking-wider flex items-center gap-2">
          <span className="text-[10px] font-mono text-[#525252]">//</span>
          LAST_24H
        </CardTitle>
      </CardHeader>
      <CardContent>
        <Table>
          <TableHeader>
            <TableRow className="border-b-[#1a1a1a] hover:bg-transparent">
              <TableHead className="text-[10px] font-mono text-[#525252] tracking-[0.1em] w-2/5">METRIC</TableHead>
              <TableHead className="text-right text-[10px] font-mono text-[#525252] tracking-[0.1em] w-3/10">{streamAName}</TableHead>
              <TableHead className="text-right text-[10px] font-mono text-[#525252] tracking-[0.1em] w-3/10">{streamBName}</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            <TableRow className="border-b-[#1a1a1a]">
              <TableCell className="text-[#8a8a8a] text-xs font-mono">UPTIME</TableCell>
              <TableCell className="text-right font-mono text-xs" style={{ color: getUptimeColor(stats.uptimeA) }}>
                {stats.uptimeA.toFixed(2)}%
              </TableCell>
              <TableCell className="text-right font-mono text-xs" style={{ color: getUptimeColor(stats.uptimeB) }}>
                {stats.uptimeB.toFixed(2)}%
              </TableCell>
            </TableRow>
            <TableRow className="border-b-[#1a1a1a]">
              <TableCell className="text-[#8a8a8a] text-xs font-mono">TOTAL_UP</TableCell>
              <TableCell className="text-right font-mono text-xs text-[#e5e5e5]">
                {formatDurationLong(stats.totalA)}
              </TableCell>
              <TableCell className="text-right font-mono text-xs text-[#e5e5e5]">
                {formatDurationLong(stats.totalB)}
              </TableCell>
            </TableRow>
            {!is28d && (
              <TableRow className="border-b-[#1a1a1a]">
                <TableCell className="text-[#8a8a8a] text-xs font-mono">RATE</TableCell>
                <TableCell className="text-right font-mono text-xs text-[#e5e5e5]">
                  {stats.rateA.toFixed(1)}<span className="text-[#525252]">/s</span>
                </TableCell>
                <TableCell className="text-right font-mono text-xs text-[#e5e5e5]">
                  {stats.rateB.toFixed(1)}<span className="text-[#525252]">/s</span>
                </TableCell>
              </TableRow>
            )}
            <TableRow className="border-b-[#1a1a1a]">
              <TableCell className="text-[#8a8a8a] text-xs font-mono">DISCONNECTS</TableCell>
              <TableCell className="text-right font-mono text-xs text-[#e5e5e5]">
                {stats.disconnectsA}
              </TableCell>
              <TableCell className="text-right font-mono text-xs text-[#e5e5e5]">
                {stats.disconnectsB}
              </TableCell>
            </TableRow>
            {!is28d && (
              <TableRow className="border-b-[#1a1a1a]">
                <TableCell className="text-[#8a8a8a] text-xs font-mono">DELIVERY_LATENCY</TableCell>
                <TableCell className="text-right font-mono text-xs text-[#e5e5e5]">
                  {stats.deliveryLatencyA > 0 ? `${stats.deliveryLatencyA.toFixed(0)}ms` : '--'}
                </TableCell>
                <TableCell className="text-right font-mono text-xs text-[#e5e5e5]">
                  {stats.deliveryLatencyB > 0 ? `${stats.deliveryLatencyB.toFixed(0)}ms` : '--'}
                </TableCell>
              </TableRow>
            )}
            {!is28d && (stats.mttrA > 0 || stats.mttrB > 0) && (
              <TableRow className="border-b-[#1a1a1a]">
                <TableCell className="text-[#8a8a8a] text-xs font-mono">MTTR</TableCell>
                <TableCell className="text-right font-mono text-xs text-[#e5e5e5]">
                  {stats.mttrA > 0 ? formatDurationLong(stats.mttrA / 1000) : '--'}
                </TableCell>
                <TableCell className="text-right font-mono text-xs text-[#e5e5e5]">
                  {stats.mttrB > 0 ? formatDurationLong(stats.mttrB / 1000) : '--'}
                </TableCell>
              </TableRow>
            )}
            {!is28d && (
              <TableRow>
                <TableCell className="text-[#8a8a8a] text-xs font-mono">MESSAGES</TableCell>
                <TableCell className="text-right font-mono text-xs text-[#e5e5e5]">
                  {stats.messagesA.toLocaleString()}
                </TableCell>
                <TableCell className="text-right font-mono text-xs text-[#e5e5e5]">
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
