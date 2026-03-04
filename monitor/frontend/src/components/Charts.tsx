import { useMemo, lazy, Suspense } from 'react'
import {
  Chart as ChartJS,
  CategoryScale,
  LinearScale,
  BarElement,
  PointElement,
  LineElement,
  Title,
  Tooltip,
  Legend,
} from 'chart.js'
import { HourlyUptime } from '../hooks/useStream'

ChartJS.register(
  CategoryScale,
  LinearScale,
  BarElement,
  PointElement,
  LineElement,
  Title,
  Tooltip,
  Legend
)

const Bar = lazy(() => import('react-chartjs-2').then(m => ({ default: m.Bar })))
const Line = lazy(() => import('react-chartjs-2').then(m => ({ default: m.Line })))

function ChartLoader() {
  return <div className="h-full flex items-center justify-center text-[#525252] font-mono text-xs">LOADING...</div>
}

interface UptimeChartProps {
  data: HourlyUptime[]
  streamAName: string
  streamBName: string
}

const terminalFont = {
  family: "'JetBrains Mono', monospace",
  size: 10,
}

const chartColors = {
  streamA: '#3fb950',
  streamABg: 'rgba(63, 185, 80, 0.6)',
  streamB: '#58a6ff',
  streamBBg: 'rgba(88, 166, 255, 0.6)',
  grid: '#1a1a1a',
  text: '#525252',
  textLight: '#8a8a8a',
}

export function UptimeChart24h({ data, streamAName, streamBName }: UptimeChartProps) {
  const chartContent = useMemo(() => {
    if (!data || data.length === 0) {
      return null
    }

    const firstHour = new Date(data[0].hour).getTime()
    
    const uptimeA = data.map((d, i) => {
      const currentHour = new Date(d.hour).getTime()
      const elapsedSeconds = (currentHour - firstHour) / 1000 + 3600
      const cumulativeDowntime = data.slice(0, i + 1).reduce((sum, item) => sum + (item.stream_a_downtime_seconds || 0), 0)
      return 100.0 - ((cumulativeDowntime / elapsedSeconds) * 100)
    })
    
    const uptimeB = data.map((d, i) => {
      const currentHour = new Date(d.hour).getTime()
      const elapsedSeconds = (currentHour - firstHour) / 1000 + 3600
      const cumulativeDowntime = data.slice(0, i + 1).reduce((sum, item) => sum + (item.stream_b_downtime_seconds || 0), 0)
      return 100.0 - ((cumulativeDowntime / elapsedSeconds) * 100)
    })

    const labels = data.map((d) => {
      const date = new Date(d.hour)
      return date.toLocaleString('en-US', {
        month: 'short',
        day: 'numeric',
        hour: '2-digit',
      })
    })

    return {
      chartData: {
        labels,
        datasets: [
          {
            label: streamAName,
            data: uptimeA,
            backgroundColor: chartColors.streamABg,
            borderColor: chartColors.streamA,
            borderWidth: 1,
            borderRadius: 2,
          },
          {
            label: streamBName,
            data: uptimeB,
            backgroundColor: chartColors.streamBBg,
            borderColor: chartColors.streamB,
            borderWidth: 1,
            borderRadius: 2,
          },
        ],
      },
      options: {
        responsive: true,
        maintainAspectRatio: false,
        plugins: {
          legend: {
            position: 'top' as const,
            align: 'end' as const,
            labels: {
              color: chartColors.textLight,
              usePointStyle: true,
              pointStyle: 'rect' as const,
              padding: 16,
              font: terminalFont,
            },
          },
          tooltip: {
            backgroundColor: '#0f0f0f',
            titleColor: '#e5e5e5',
            bodyColor: chartColors.textLight,
            borderColor: '#252525',
            borderWidth: 1,
            cornerRadius: 2,
            padding: 10,
            titleFont: terminalFont,
            bodyFont: terminalFont,
          },
        },
        scales: {
          x: {
            ticks: { color: chartColors.text, maxTicksLimit: 12, font: terminalFont },
            grid: { display: false },
          },
          y: {
            ticks: { color: chartColors.text, callback: (v: number | string) => v + '%', font: terminalFont },
            grid: { color: chartColors.grid },
            min: 0,
            max: 100,
          },
        },
      },
    }
  }, [data, streamAName, streamBName])

  if (!chartContent) {
    return <div className="h-[280px] flex items-center justify-center text-[#525252] font-mono text-xs">NO_DATA</div>
  }

  return (
    <div className="chart-card bg-[#0f0f0f] border border-[#1a1a1a] rounded-sm p-6 mb-6 transition-all duration-200 hover:border-[#252525]">
      <div className="chart-header flex justify-between items-center mb-4">
        <div className="flex items-center gap-2">
          <span className="text-[10px] font-mono text-[#525252]">//</span>
          <h3 className="text-xs font-semibold text-[#8a8a8a] tracking-wider font-[inherit]">
            UPTIME_24H
          </h3>
        </div>
      </div>
      <div className="h-[280px] relative w-full">
        <Suspense fallback={<ChartLoader />}>
          <Bar data={chartContent.chartData} options={chartContent.options} />
        </Suspense>
      </div>
    </div>
  )
}

export function UptimeChart28d({ data, streamAName, streamBName }: UptimeChartProps) {
  const chartContent = useMemo(() => {
    if (!data || data.length === 0) {
      return null
    }

    const dailyUptime: Record<string, { a: number; b: number; count: number }> = {}
    
    data.forEach((d) => {
      const day = new Date(d.hour).toISOString().split('T')[0]
      if (!dailyUptime[day]) {
        dailyUptime[day] = { a: 0, b: 0, count: 0 }
      }
      dailyUptime[day].a += d.stream_a_downtime_seconds || 0
      dailyUptime[day].b += d.stream_b_downtime_seconds || 0
      dailyUptime[day].count++
    })
    
    const days = Object.keys(dailyUptime).slice(-28)
    const firstDayTimestamp = new Date(days[0]).getTime()

    const uptimeA = days.map((d, i) => {
      const currentDayTimestamp = new Date(d).getTime()
      const elapsedSeconds = (currentDayTimestamp - firstDayTimestamp) / 1000 + (i + 1) * 86400
      const cumulativeDowntime = days.slice(0, i + 1).reduce((sum, day) => sum + dailyUptime[day].a, 0)
      return 100.0 - ((cumulativeDowntime / elapsedSeconds) * 100)
    })
    
    const uptimeB = days.map((d, i) => {
      const currentDayTimestamp = new Date(d).getTime()
      const elapsedSeconds = (currentDayTimestamp - firstDayTimestamp) / 1000 + (i + 1) * 86400
      const cumulativeDowntime = days.slice(0, i + 1).reduce((sum, day) => sum + dailyUptime[day].b, 0)
      return 100.0 - ((cumulativeDowntime / elapsedSeconds) * 100)
    })

    return {
      chartData: {
        labels: days,
        datasets: [
          {
            label: streamAName,
            data: uptimeA,
            backgroundColor: chartColors.streamABg,
            borderColor: chartColors.streamA,
            borderWidth: 1,
            borderRadius: 2,
          },
          {
            label: streamBName,
            data: uptimeB,
            backgroundColor: chartColors.streamBBg,
            borderColor: chartColors.streamB,
            borderWidth: 1,
            borderRadius: 2,
          },
        ],
      },
      options: {
        responsive: true,
        maintainAspectRatio: false,
        plugins: {
          legend: {
            position: 'top' as const,
            align: 'end' as const,
            labels: {
              color: chartColors.textLight,
              usePointStyle: true,
              pointStyle: 'rect' as const,
              padding: 16,
              font: terminalFont,
            },
          },
          tooltip: {
            backgroundColor: '#0f0f0f',
            titleColor: '#e5e5e5',
            bodyColor: chartColors.textLight,
            borderColor: '#252525',
            borderWidth: 1,
            cornerRadius: 2,
            padding: 10,
            titleFont: terminalFont,
            bodyFont: terminalFont,
          },
        },
        scales: {
          x: {
            ticks: { color: chartColors.text, maxTicksLimit: 12, font: terminalFont },
            grid: { display: false },
          },
          y: {
            ticks: { color: chartColors.text, callback: (v: number | string) => v + '%', font: terminalFont },
            grid: { color: chartColors.grid },
            min: 0,
            max: 100,
          },
        },
      },
    }
  }, [data, streamAName, streamBName])

  if (!chartContent) {
    return <div className="h-[280px] flex items-center justify-center text-[#525252] font-mono text-xs">NO_DATA</div>
  }

  return (
    <div className="chart-card bg-[#0f0f0f] border border-[#1a1a1a] rounded-sm p-6 mb-6 transition-all duration-200 hover:border-[#252525]">
      <div className="chart-header flex justify-between items-center mb-4">
        <div className="flex items-center gap-2">
          <span className="text-[10px] font-mono text-[#525252]">//</span>
          <h3 className="text-xs font-semibold text-[#8a8a8a] tracking-wider font-[inherit]">
            UPTIME_28D
          </h3>
        </div>
      </div>
      <div className="h-[280px] relative w-full">
        <Suspense fallback={<ChartLoader />}>
          <Bar data={chartContent.chartData} options={chartContent.options} />
        </Suspense>
      </div>
    </div>
  )
}

export function RateChart({ data, streamAName, streamBName }: UptimeChartProps) {
  const chartContent = useMemo(() => {
    if (!data || data.length === 0) {
      return null
    }

    const messagesA = data.map((d) => (d.stream_a_messages || 0) / 3600)
    const messagesB = data.map((d) => (d.stream_b_messages || 0) / 3600)

    const labels = data.map((d) => {
      const date = new Date(d.hour)
      return date.toLocaleString('en-US', {
        month: 'short',
        day: 'numeric',
        hour: '2-digit',
      })
    })

    return {
      chartData: {
        labels,
        datasets: [
          {
            label: streamAName,
            data: messagesA,
            borderColor: chartColors.streamA,
            backgroundColor: 'transparent',
            tension: 0.2,
            borderWidth: 2,
            pointRadius: 0,
            pointHoverRadius: 4,
          },
          {
            label: streamBName,
            data: messagesB,
            borderColor: chartColors.streamB,
            backgroundColor: 'transparent',
            tension: 0.2,
            borderWidth: 2,
            pointRadius: 0,
            pointHoverRadius: 4,
          },
        ],
      },
      options: {
        responsive: true,
        maintainAspectRatio: false,
        plugins: {
          legend: {
            position: 'top' as const,
            align: 'end' as const,
            labels: {
              color: chartColors.textLight,
              usePointStyle: true,
              pointStyle: 'circle' as const,
              padding: 16,
              font: terminalFont,
            },
          },
          tooltip: {
            backgroundColor: '#0f0f0f',
            titleColor: '#e5e5e5',
            bodyColor: chartColors.textLight,
            borderColor: '#252525',
            borderWidth: 1,
            cornerRadius: 2,
            padding: 10,
            titleFont: terminalFont,
            bodyFont: terminalFont,
          },
        },
        scales: {
          x: {
            ticks: { color: chartColors.text, maxTicksLimit: 12, font: terminalFont },
            grid: { display: false },
          },
          y: {
            ticks: { color: chartColors.text, callback: (v: number | string) => v + '/s', font: terminalFont },
            grid: { color: chartColors.grid },
          },
        },
      },
    }
  }, [data, streamAName, streamBName])

  if (!chartContent) {
    return <div className="h-[220px] flex items-center justify-center text-[#525252] font-mono text-xs">NO_DATA</div>
  }

  return (
    <div className="chart-card bg-[#0f0f0f] border border-[#1a1a1a] rounded-sm p-6 mb-6 transition-all duration-200 hover:border-[#252525]">
      <div className="chart-header flex justify-between items-center mb-4">
        <div className="flex items-center gap-2">
          <span className="text-[10px] font-mono text-[#525252]">//</span>
          <h3 className="text-xs font-semibold text-[#8a8a8a] tracking-wider font-[inherit]">
            MESSAGE_RATE_24H
          </h3>
        </div>
      </div>
      <div className="h-[220px] relative w-full">
        <Suspense fallback={<ChartLoader />}>
          <Line data={chartContent.chartData} options={chartContent.options} />
        </Suspense>
      </div>
    </div>
  )
}
