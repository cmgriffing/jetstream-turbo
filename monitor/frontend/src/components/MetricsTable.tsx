import { HourlyUptime } from '../hooks/useStream'

interface MetricsTableProps {
  title: string
  icon: string
  data: HourlyUptime[]
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
  if (percentage >= 99) return 'good text-[#22c55e]'
  if (percentage >= 95) return 'warning text-[#eab308]'
  return 'bad text-[#ef4444]'
}

function calculateStats(data: HourlyUptime[], _hours: number) {
  if (data.length === 0) return null

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

  const firstHour = new Date(data[0].hour)
  const lastHour = new Date(data[data.length - 1].hour)
  const actualSpanSeconds = (lastHour.getTime() - firstHour.getTime()) / 1000 + 3600

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

export function MetricsTable({ title, icon, data, streamAName, streamBName }: MetricsTableProps) {
  const stats = calculateStats(data, data.length > 24 * 7 ? 28 * 24 : 24)
  const is28d = data.length > 24

  if (!stats) {
    return (
      <div className="metric-card bg-[#141414] border border-[#1f1f1f] rounded-3xl p-8 transition-all duration-300 hover:border-[#2a2a2a]">
        <h3 className="text-[0.875rem] font-semibold mb-8 flex items-center gap-2">
          <span className="text-[1rem]">{icon}</span> {title}
        </h3>
        <p className="text-[#525252] text-sm">No data available</p>
      </div>
    )
  }

  return (
    <div className="metric-card bg-[#141414] border border-[#1f1f1f] rounded-3xl p-8 transition-all duration-300 hover:border-[#2a2a2a] hover:shadow-[0_4px_24px_rgba(0,0,0,0.3)]">
      <h3 className="text-[0.875rem] font-semibold mb-8 flex items-center gap-2">
        <span className="text-[1rem]">{icon}</span> {title}
      </h3>
      <table className="w-full border-collapse">
        <thead>
          <tr>
            <th className="text-left text-[0.6875rem] text-[#525252] uppercase tracking-wider py-4 border-b border-[#1f1f1f] font-medium w-2/5">
              Metric
            </th>
            <th className="text-right text-[0.6875rem] text-[#525252] uppercase tracking-wider py-4 border-b border-[#1f1f1f] font-medium w-3/10">
              {streamAName}
            </th>
            <th className="text-right text-[0.6875rem] text-[#525252] uppercase tracking-wider py-4 border-b border-[#1f1f1f] font-medium w-3/10">
              {streamBName}
            </th>
          </tr>
        </thead>
        <tbody>
          <tr className="transition-colors duration-150 hover:bg-[#181818]">
            <td className="py-5 text-[#a3a3a3] text-[0.9375rem] border-b border-[#1f1f1f]">Uptime</td>
            <td className={`py-5 text-right text-[0.9375rem] font-semibold tabular-nums border-b border-[#1f1f1f] ${getUptimeClass(stats.uptimeA)}`}>
              {stats.uptimeA.toFixed(2)}%
            </td>
            <td className={`py-5 text-right text-[0.9375rem] font-semibold tabular-nums border-b border-[#1f1f1f] ${getUptimeClass(stats.uptimeB)}`}>
              {stats.uptimeB.toFixed(2)}%
            </td>
          </tr>
          <tr className="transition-colors duration-150 hover:bg-[#181818]">
            <td className="py-5 text-[#a3a3a3] text-[0.9375rem] border-b border-[#1f1f1f]">Total Up Time</td>
            <td className="py-5 text-right text-[0.9375rem] font-semibold tabular-nums text-[#e5e5e5] border-b border-[#1f1f1f]">
              {formatDurationLong(stats.totalA)}
            </td>
            <td className="py-5 text-right text-[0.9375rem] font-semibold tabular-nums text-[#e5e5e5] border-b border-[#1f1f1f]">
              {formatDurationLong(stats.totalB)}
            </td>
          </tr>
          {!is28d && (
            <tr className="transition-colors duration-150 hover:bg-[#181818]">
              <td className="py-5 text-[#a3a3a3] text-[0.9375rem] border-b border-[#1f1f1f]">Messages/sec</td>
              <td className="py-5 text-right text-[0.9375rem] font-semibold tabular-nums text-[#e5e5e5] border-b border-[#1f1f1f]">
                {stats.rateA.toFixed(1)}/s
              </td>
              <td className="py-5 text-right text-[0.9375rem] font-semibold tabular-nums text-[#e5e5e5] border-b border-[#1f1f1f]">
                {stats.rateB.toFixed(1)}/s
              </td>
            </tr>
          )}
          <tr className="transition-colors duration-150 hover:bg-[#181818]">
            <td className="py-5 text-[#a3a3a3] text-[0.9375rem] border-b border-[#1f1f1f]">Disconnects</td>
            <td className="py-5 text-right text-[0.9375rem] font-semibold tabular-nums text-[#e5e5e5] border-b border-[#1f1f1f]">
              {stats.disconnectsA}
            </td>
            <td className="py-5 text-right text-[0.9375rem] font-semibold tabular-nums text-[#e5e5e5] border-b border-[#1f1f1f]">
              {stats.disconnectsB}
            </td>
          </tr>
          {!is28d && (
            <tr className="transition-colors duration-150 hover:bg-[#181818]">
              <td className="py-5 text-[#a3a3a3] text-[0.9375rem]">Total Messages</td>
              <td className="py-5 text-right text-[0.9375rem] font-semibold tabular-nums text-[#e5e5e5]">
                {stats.messagesA.toLocaleString()}
              </td>
              <td className="py-5 text-right text-[0.9375rem] font-semibold tabular-nums text-[#e5e5e5]">
                {stats.messagesB.toLocaleString()}
              </td>
            </tr>
          )}
        </tbody>
      </table>
    </div>
  )
}
