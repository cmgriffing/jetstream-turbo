use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    pub stream_a_url: String,
    #[serde(default = "default_stream_a_name")]
    pub stream_a_name: String,
    #[serde(default = "default_stream_b_name")]
    pub stream_b_url: String,

    pub stream_b_name: String,
    #[serde(default = "default_bind")]
    pub bind_address: String,
    #[serde(default = "default_database")]
    pub database_url: String,
    #[serde(default = "default_stream_idle_timeout_seconds")]
    pub stream_idle_timeout_seconds: u64,
    #[serde(default = "default_connection_timeout_seconds")]
    pub connection_timeout_seconds: u64,
    #[serde(default = "default_heartbeat_interval_seconds")]
    pub heartbeat_interval_seconds: u64,
    #[serde(default = "default_transport_liveness_deadline_seconds")]
    pub transport_liveness_deadline_seconds: u64,
    #[serde(default = "default_reconnect_backoff_min_seconds")]
    pub reconnect_backoff_min_seconds: u64,
    #[serde(default = "default_reconnect_backoff_max_seconds")]
    pub reconnect_backoff_max_seconds: u64,
    #[serde(default = "default_incident_retention_days")]
    pub incident_retention_days: u64,
    #[serde(default = "default_monitor_release")]
    pub monitor_release: String,
    #[serde(default = "default_api_server_url")]
    pub api_server_url: String,
    #[serde(default = "default_live_lag_threshold_seconds")]
    pub live_lag_threshold_seconds: u64,
    #[serde(default = "default_watermark_skew_threshold_seconds")]
    pub watermark_skew_threshold_seconds: u64,
    #[serde(default = "default_comparison_horizon_seconds")]
    pub comparison_horizon_seconds: u64,
    #[serde(default = "default_comparison_bucket_width_seconds")]
    pub comparison_bucket_width_seconds: u64,
    #[serde(default = "default_comparison_settlement_allowance_seconds")]
    pub comparison_settlement_allowance_seconds: u64,
    #[serde(default = "default_diagnostics_log_path")]
    pub diagnostics_log_path: String,
    #[serde(default = "default_diagnostics_log_max_bytes")]
    pub diagnostics_log_max_bytes: u64,
}

fn default_stream_a_name() -> String {
    "Stream A".to_string()
}

fn default_stream_b_name() -> String {
    "Stream B".to_string()
}

fn default_bind() -> String {
    "0.0.0.0:3001".to_string()
}

fn default_database() -> String {
    "sqlite://monitor.db?mode=rwc".to_string()
}

fn default_stream_idle_timeout_seconds() -> u64 {
    30
}

fn default_connection_timeout_seconds() -> u64 {
    15
}

fn default_heartbeat_interval_seconds() -> u64 {
    20
}

fn default_transport_liveness_deadline_seconds() -> u64 {
    60
}

fn default_reconnect_backoff_min_seconds() -> u64 {
    1
}

fn default_reconnect_backoff_max_seconds() -> u64 {
    30
}

fn default_incident_retention_days() -> u64 {
    90
}

fn default_monitor_release() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

fn default_api_server_url() -> String {
    "http://localhost:3001".to_string()
}

fn default_live_lag_threshold_seconds() -> u64 {
    30
}

fn default_watermark_skew_threshold_seconds() -> u64 {
    30
}

fn default_comparison_horizon_seconds() -> u64 {
    300
}

fn default_comparison_bucket_width_seconds() -> u64 {
    5
}

fn default_comparison_settlement_allowance_seconds() -> u64 {
    10
}

fn default_diagnostics_log_path() -> String {
    "./monitor-diagnostics.log".to_string()
}

fn default_diagnostics_log_max_bytes() -> u64 {
    1048576
}

impl Settings {
    pub fn load() -> Result<Self> {
        dotenvy::dotenv().ok();

        let settings = config::Config::builder()
            .set_default("bind_address", default_bind())?
            .set_default("database_url", default_database())?
            .set_default("stream_a_name", default_stream_a_name())?
            .set_default("stream_b_name", default_stream_b_name())?
            .set_default(
                "stream_idle_timeout_seconds",
                default_stream_idle_timeout_seconds(),
            )?
            .set_default(
                "connection_timeout_seconds",
                default_connection_timeout_seconds(),
            )?
            .set_default(
                "heartbeat_interval_seconds",
                default_heartbeat_interval_seconds(),
            )?
            .set_default(
                "transport_liveness_deadline_seconds",
                default_transport_liveness_deadline_seconds(),
            )?
            .set_default(
                "reconnect_backoff_min_seconds",
                default_reconnect_backoff_min_seconds(),
            )?
            .set_default(
                "reconnect_backoff_max_seconds",
                default_reconnect_backoff_max_seconds(),
            )?
            .set_default("incident_retention_days", default_incident_retention_days())?
            .set_default("monitor_release", default_monitor_release())?
            .set_default("api_server_url", default_api_server_url())?
            .set_default(
                "live_lag_threshold_seconds",
                default_live_lag_threshold_seconds(),
            )?
            .set_default(
                "watermark_skew_threshold_seconds",
                default_watermark_skew_threshold_seconds(),
            )?
            .set_default(
                "comparison_horizon_seconds",
                default_comparison_horizon_seconds(),
            )?
            .set_default(
                "comparison_bucket_width_seconds",
                default_comparison_bucket_width_seconds(),
            )?
            .set_default(
                "comparison_settlement_allowance_seconds",
                default_comparison_settlement_allowance_seconds(),
            )?
            .set_default("diagnostics_log_path", default_diagnostics_log_path())?
            .set_default(
                "diagnostics_log_max_bytes",
                default_diagnostics_log_max_bytes(),
            )?
            .add_source(config::Environment::default())
            .build()?;

        let settings: Self = settings.try_deserialize()?;
        validate_event_time_thresholds(
            settings.live_lag_threshold_seconds,
            settings.watermark_skew_threshold_seconds,
        )?;
        validate_comparison_settings(
            settings.comparison_horizon_seconds,
            settings.comparison_bucket_width_seconds,
            settings.comparison_settlement_allowance_seconds,
        )?;
        validate_liveness_settings(
            settings.heartbeat_interval_seconds,
            settings.transport_liveness_deadline_seconds,
        )?;
        validate_backoff_settings(
            settings.reconnect_backoff_min_seconds,
            settings.reconnect_backoff_max_seconds,
        )?;
        if settings.incident_retention_days == 0 {
            anyhow::bail!("incident_retention_days must be greater than zero");
        }
        if settings.monitor_release.trim().is_empty() {
            anyhow::bail!("monitor_release must not be empty");
        }
        if !settings.api_server_url.starts_with("http") {
            anyhow::bail!("api_server_url must be an HTTP(S) URL");
        }
        Ok(settings)
    }
}

fn validate_liveness_settings(
    heartbeat_seconds: u64,
    liveness_deadline_seconds: u64,
) -> Result<()> {
    if heartbeat_seconds == 0 {
        anyhow::bail!("heartbeat_interval_seconds must be greater than zero");
    }
    if liveness_deadline_seconds <= heartbeat_seconds {
        anyhow::bail!(
            "transport_liveness_deadline_seconds must be greater than heartbeat_interval_seconds"
        );
    }
    Ok(())
}

fn validate_backoff_settings(min_seconds: u64, max_seconds: u64) -> Result<()> {
    if min_seconds == 0 {
        anyhow::bail!("reconnect_backoff_min_seconds must be greater than zero");
    }
    if max_seconds < min_seconds {
        anyhow::bail!(
            "reconnect_backoff_max_seconds must not be less than reconnect_backoff_min_seconds"
        );
    }
    if max_seconds == 0 {
        anyhow::bail!("reconnect_backoff_max_seconds must be greater than zero");
    }
    Ok(())
}

fn validate_comparison_settings(horizon: u64, width: u64, settlement: u64) -> Result<()> {
    if horizon == 0 || width == 0 {
        anyhow::bail!("comparison horizon and bucket width must be greater than zero");
    }
    if width > horizon {
        anyhow::bail!("comparison bucket width must not exceed comparison horizon");
    }
    if settlement >= horizon {
        anyhow::bail!("comparison settlement allowance must be less than comparison horizon");
    }
    Ok(())
}

fn validate_event_time_thresholds(
    live_lag_seconds: u64,
    watermark_skew_seconds: u64,
) -> Result<()> {
    if live_lag_seconds == 0 {
        anyhow::bail!("live_lag_threshold_seconds must be greater than zero");
    }
    if watermark_skew_seconds == 0 {
        anyhow::bail!("watermark_skew_threshold_seconds must be greater than zero");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_time_threshold_defaults_align_with_useful_delivery_timeout() {
        assert_eq!(default_live_lag_threshold_seconds(), 30);
        assert_eq!(default_watermark_skew_threshold_seconds(), 30);
        assert_eq!(default_stream_idle_timeout_seconds(), 30);
        assert_eq!(default_comparison_horizon_seconds(), 300);
        assert_eq!(default_comparison_bucket_width_seconds(), 5);
        assert_eq!(default_comparison_settlement_allowance_seconds(), 10);
    }

    #[test]
    fn event_time_thresholds_must_be_positive() {
        assert!(validate_event_time_thresholds(0, 30).is_err());
        assert!(validate_event_time_thresholds(30, 0).is_err());
        assert!(validate_event_time_thresholds(30, 30).is_ok());
    }

    #[test]
    fn comparison_settings_are_bounded_and_coherent() {
        assert!(validate_comparison_settings(300, 5, 10).is_ok());
        assert!(validate_comparison_settings(0, 5, 10).is_err());
        assert!(validate_comparison_settings(300, 0, 10).is_err());
        assert!(validate_comparison_settings(10, 20, 1).is_err());
        assert!(validate_comparison_settings(10, 5, 10).is_err());
    }

    #[test]
    fn liveness_settings_require_deadline_beyond_heartbeat() {
        assert!(validate_liveness_settings(20, 60).is_ok());
        assert!(validate_liveness_settings(0, 60).is_err());
        assert!(validate_liveness_settings(60, 60).is_err());
        assert!(validate_liveness_settings(20, 10).is_err());
    }

    #[test]
    fn backoff_settings_are_bounded_and_coherent() {
        assert!(validate_backoff_settings(1, 30).is_ok());
        assert!(validate_backoff_settings(0, 30).is_err());
        assert!(validate_backoff_settings(40, 30).is_err());
        assert!(validate_backoff_settings(1, 0).is_err());
    }

    #[test]
    fn defaults_follow_design_document() {
        assert_eq!(default_heartbeat_interval_seconds(), 20);
        assert_eq!(default_transport_liveness_deadline_seconds(), 60);
        assert_eq!(default_reconnect_backoff_min_seconds(), 1);
        assert_eq!(default_reconnect_backoff_max_seconds(), 30);
        assert_eq!(default_incident_retention_days(), 90);
        assert_eq!(default_monitor_release(), "0.1.0");
    }
}
