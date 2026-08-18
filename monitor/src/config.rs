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
    #[serde(default = "default_live_lag_threshold_seconds")]
    pub live_lag_threshold_seconds: u64,
    #[serde(default = "default_watermark_skew_threshold_seconds")]
    pub watermark_skew_threshold_seconds: u64,
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

fn default_live_lag_threshold_seconds() -> u64 {
    30
}

fn default_watermark_skew_threshold_seconds() -> u64 {
    30
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
                "live_lag_threshold_seconds",
                default_live_lag_threshold_seconds(),
            )?
            .set_default(
                "watermark_skew_threshold_seconds",
                default_watermark_skew_threshold_seconds(),
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
        Ok(settings)
    }
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
    }

    #[test]
    fn event_time_thresholds_must_be_positive() {
        assert!(validate_event_time_thresholds(0, 30).is_err());
        assert!(validate_event_time_thresholds(30, 0).is_err());
        assert!(validate_event_time_thresholds(30, 30).is_ok());
    }
}
