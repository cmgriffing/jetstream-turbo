import { AlertTriangle, RefreshCw } from "lucide-react"
import { useEffect, useState } from "react"
import { ConnectionStatus } from "@/hooks/useStream"

interface ConnectionBannerProps {
  status: ConnectionStatus
}

export function ConnectionBanner({ status }: ConnectionBannerProps) {
  const [showBanner, setShowBanner] = useState(false)

  useEffect(() => {
    if (status === 'disconnected') {
      const timer = setTimeout(() => {
        setShowBanner(true)
      }, 3000)
      return () => clearTimeout(timer)
    } else {
      setShowBanner(false)
    }
  }, [status])

  if (!showBanner) return null

  return (
    <div className="fixed top-0 left-0 right-0 z-50 flex items-center justify-center gap-2 px-4 py-3 bg-destructive text-destructive-foreground" role="status" aria-live="polite">
      <AlertTriangle className="w-5 h-5" />
      <span className="font-medium">Unable to connect to server. Retrying...</span>
      <RefreshCw className="w-4 h-4 animate-spin" />
    </div>
  )
}
