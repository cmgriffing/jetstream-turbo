import { AlertTriangle, RefreshCw } from "lucide-react";
import { useEffect, useState } from "react";
import { ConnectionStatus } from "@/hooks/useStream";

interface ConnectionBannerProps {
  status: ConnectionStatus;
}

export function ConnectionBanner({ status }: ConnectionBannerProps) {
  const [showBanner, setShowBanner] = useState(false);

  useEffect(() => {
    if (status === "disconnected") {
      const timer = setTimeout(() => {
        setShowBanner(true);
      }, 3000);
      return () => clearTimeout(timer);
    }

    setShowBanner(false);
  }, [status]);

  if (!showBanner) return null;

  return (
    <div className="monitor-connection-banner" role="status" aria-live="polite">
      <span className="monitor-connection-banner-alert" aria-hidden="true">
        <AlertTriangle className="h-4 w-4" />
      </span>
      <span className="monitor-connection-banner-text">
        Unable to connect to server. Retrying.
      </span>
      <RefreshCw className="monitor-connection-banner-spinner h-3.5 w-3.5 animate-spin" />
    </div>
  );
}
