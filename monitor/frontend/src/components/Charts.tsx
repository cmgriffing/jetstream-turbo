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
import { Bar, Line } from 'react-chartjs-2'
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

interface UptimeChartProps {
  data: HourlyUptime[]
  streamAName: string
  streamBName: string
}

export function UptimeChart24h({ data, streamAName, streamBName }: UptimeChartProps) {
  if (!data || data.length === 0) {
    return <div className="h-[280px] flex items-center justify-center text-[#525252]">No data</div>
  }

  const firstHour = new Date(data[0].hour).getTime()
  
  const uptimeA = data.map((d, i) => {
    const currentHour = new Date(d.hour).getTime()
    const elapsedSeconds = (currentHour - firstHour) / 1000 + 3600
    const cumulativeSeconds = data.slice(0, i + 1).reduce((sum, item) => sum + (item.stream_a_seconds || 0), 0)
    return (cumulativeSeconds / elapsedSeconds) * 100
  })
  
  const uptimeB = data.map((d, i) => {
    const currentHour = new Date(d.hour).getTime()
    const elapsedSeconds = (currentHour - firstHour) / 1000 + 3600
    const cumulativeSeconds = data.slice(0, i + 1).reduce((sum, item) => sum + (item.stream_b_seconds || 0), 0)
    return (cumulativeSeconds / elapsedSeconds) * 100
  })

  const labels = data.map((d) => {
    const date = new Date(d.hour)
    return date.toLocaleString('en-US', {
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
    })
  })

  const chartData = {
    labels,
    datasets: [
      {
        label: streamAName,
        data: uptimeA,
        backgroundColor: 'rgba(34, 197, 94, 0.7)',
        borderColor: '#22c55e',
        borderWidth: 1,
        borderRadius: 4,
      },
      {
        label: streamBName,
        data: uptimeB,
        backgroundColor: 'rgba(59, 130, 246, 0.7)',
        borderColor: '#3b82f6',
        borderWidth: 1,
        borderRadius: 4,
      },
    ],
  }

  const options = {
    responsive: true,
    maintainAspectRatio: false,
    plugins: {
      legend: {
        position: 'top' as const,
        align: 'end' as const,
        labels: {
          color: '#888',
          usePointStyle: true,
          pointStyle: 'rect' as const,
          padding: 20,
          font: { size: 11 },
        },
      },
      tooltip: {
        backgroundColor: '#1a1a1a',
        titleColor: '#e0e0e0',
        bodyColor: '#888',
        borderColor: '#333',
        borderWidth: 1,
        cornerRadius: 8,
        padding: 12,
      },
    },
    scales: {
      x: {
        ticks: { color: '#666', maxTicksLimit: 12, font: { size: 10 } },
        grid: { display: false },
      },
      y: {
        ticks: { color: '#666', callback: (v: number | string) => v + '%', font: { size: 10 } },
        grid: { color: '#1a1a1a' },
        min: 0,
        max: 100,
      },
    },
  }

  return (
    <div className="chart-card bg-[#141414] border border-[#1f1f1f] rounded-2xl p-6 mb-5 transition-all duration-300 hover:border-[#2a2a2a] hover:shadow-[0_4px_24px_rgba(0,0,0,0.3)]">
      <div className="chart-header flex justify-between items-center mb-5">
        <h3 className="text-[0.875rem] font-semibold flex items-center gap-2">
          <span className="text-[1rem]">📈</span> Uptime (24h)
        </h3>
      </div>
      <div className="h-[280px] relative w-full">
        <Bar data={chartData} options={options} />
      </div>
    </div>
  )
}

export function UptimeChart28d({ data, streamAName, streamBName }: UptimeChartProps) {
  if (!data || data.length === 0) {
    return <div className="h-[280px] flex items-center justify-center text-[#525252]">No data</div>
  }

  const dailyUptime: Record<string, { a: number; b: number; count: number }> = {}
  
  data.forEach((d) => {
    const day = new Date(d.hour).toISOString().split('T')[0]
    if (!dailyUptime[day]) {
      dailyUptime[day] = { a: 0, b: 0, count: 0 }
    }
    dailyUptime[day].a += d.stream_a_seconds
    dailyUptime[day].b += d.stream_b_seconds
    dailyUptime[day].count++
  })
  
  const days = Object.keys(dailyUptime).slice(-28)
  const firstDayTimestamp = new Date(days[0]).getTime()

  const uptimeA = days.map((d, i) => {
    const currentDayTimestamp = new Date(d).getTime()
    const elapsedSeconds = (currentDayTimestamp - firstDayTimestamp) / 1000 + (i + 1) * 86400
    const cumulativeSeconds = days.slice(0, i + 1).reduce((sum, day) => sum + dailyUptime[day].a, 0)
    return (cumulativeSeconds / elapsedSeconds) * 100
  })
  
  const uptimeB = days.map((d, i) => {
    const currentDayTimestamp = new Date(d).getTime()
    const elapsedSeconds = (currentDayTimestamp - firstDayTimestamp) / 1000 + (i + 1) * 86400
    const cumulativeSeconds = days.slice(0, i + 1).reduce((sum, day) => sum + dailyUptime[day].b, 0)
    return (cumulativeSeconds / elapsedSeconds) * 100
  })

  const chartData = {
    labels: days,
    datasets: [
      {
        label: streamAName,
        data: uptimeA,
        backgroundColor: 'rgba(34, 197, 94, 0.7)',
        borderColor: '#22c55e',
        borderWidth: 1,
        borderRadius: 4,
      },
      {
        label: streamBName,
        data: uptimeB,
        backgroundColor: 'rgba(59, 130, 246, 0.7)',
        borderColor: '#3b82f6',
        borderWidth: 1,
        borderRadius: 4,
      },
    ],
  }

  const options = {
    responsive: true,
    maintainAspectRatio: false,
    plugins: {
      legend: {
        position: 'top' as const,
        align: 'end' as const,
        labels: {
          color: '#888',
          usePointStyle: true,
          pointStyle: 'rect' as const,
          padding: 20,
          font: { size: 11 },
        },
      },
      tooltip: {
        backgroundColor: '#1a1a1a',
        titleColor: '#e0e0e0',
        bodyColor: '#888',
        borderColor: '#333',
        borderWidth: 1,
        cornerRadius: 8,
        padding: 12,
      },
    },
    scales: {
      x: {
        ticks: { color: '#666', maxTicksLimit: 12, font: { size: 10 } },
        grid: { display: false },
      },
      y: {
        ticks: { color: '#666', callback: (v: number | string) => v + '%', font: { size: 10 } },
        grid: { color: '#1a1a1a' },
        min: 0,
        max: 100,
      },
    },
  }

  return (
    <div className="chart-card bg-[#141414] border border-[#1f1f1f] rounded-2xl p-6 mb-5 transition-all duration-300 hover:border-[#2a2a2a] hover:shadow-[0_4px_24px_rgba(0,0,0,0.3)]">
      <div className="chart-header flex justify-between items-center mb-5">
        <h3 className="text-[0.875rem] font-semibold flex items-center gap-2">
          <span className="text-[1rem]">📈</span> Uptime (28 days)
        </h3>
      </div>
      <div className="h-[280px] relative w-full">
        <Bar data={chartData} options={options} />
      </div>
    </div>
  )
}

export function RateChart({ data, streamAName, streamBName }: UptimeChartProps) {
  if (!data || data.length === 0) {
    return <div className="h-[220px] flex items-center justify-center text-[#525252]">No data</div>
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

  const chartData = {
    labels,
    datasets: [
      {
        label: streamAName,
        data: messagesA,
        borderColor: '#22c55e',
        backgroundColor: 'transparent',
        tension: 0.3,
      },
      {
        label: streamBName,
        data: messagesB,
        borderColor: '#3b82f6',
        backgroundColor: 'transparent',
        tension: 0.3,
      },
    ],
  }

  const options = {
    responsive: true,
    maintainAspectRatio: false,
    plugins: {
      legend: {
        position: 'top' as const,
        align: 'end' as const,
        labels: {
          color: '#888',
          usePointStyle: true,
          pointStyle: 'circle' as const,
          padding: 20,
          font: { size: 11 },
        },
      },
      tooltip: {
        backgroundColor: '#1a1a1a',
        titleColor: '#e0e0e0',
        bodyColor: '#888',
        borderColor: '#333',
        borderWidth: 1,
        cornerRadius: 8,
        padding: 12,
      },
    },
    scales: {
      x: {
        ticks: { color: '#666', maxTicksLimit: 12, font: { size: 10 } },
        grid: { display: false },
      },
      y: {
        ticks: { color: '#666', callback: (v: number | string) => v + '/s', font: { size: 10 } },
        grid: { color: '#1a1a1a' },
      },
    },
  }

  return (
    <div className="chart-card bg-[#141414] border border-[#1f1f1f] rounded-2xl p-6 mb-5 transition-all duration-300 hover:border-[#2a2a2a] hover:shadow-[0_4px_24px_rgba(0,0,0,0.3)]">
      <div className="chart-header flex justify-between items-center mb-5">
        <h3 className="text-[0.875rem] font-semibold flex items-center gap-2">
          <span className="text-[1rem]">💬</span> Message Rate (24h)
        </h3>
      </div>
      <div className="h-[220px] relative w-full">
        <Line data={chartData} options={options} />
      </div>
    </div>
  )
}
