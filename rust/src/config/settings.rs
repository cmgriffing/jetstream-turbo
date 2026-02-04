use anyhow::Result;
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    // API Configuration
    pub graze_api_base_url: String,
    pub stream_name: String,
    pub turbo_credential_secret: String,

    // Jetstream Configuration
    #[serde(default = "default_jetstream_hosts")]
    pub jetstream_hosts: Vec<String>,
    pub wanted_collections: String,

    // Redis Configuration
    pub redis_url: String,
    pub stream_name_redis: String,
    pub trim_maxlen: Option<usize>,

    // Storage Configuration
    pub db_dir: String,
    pub s3_bucket: String,
    pub s3_region: String,
    pub rotation_minutes: u64,

    // HTTP Server Configuration
    pub http_port: u16,

    // Performance Configuration
    pub batch_size: usize,
    pub max_concurrent_requests: usize,
    pub cache_size_users: usize,
    pub cache_size_posts: usize,

    // Retry Configuration
    pub max_retries: u32,
    pub retry_base_delay: Duration,

    // Metrics Configuration
    pub statsd_host: Option<String>,
    pub statsd_port: Option<u16>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            graze_api_base_url: "https://api.graze.social".to_string(),
            stream_name: "".to_string(),
            turbo_credential_secret: "".to_string(),
            jetstream_hosts: default_jetstream_hosts(),
            wanted_collections: "app.bsky.feed.post".to_string(),
            redis_url: "redis://localhost:6379".to_string(),
            stream_name_redis: "hydrated_jetstream".to_string(),
            trim_maxlen: Some(100),
            db_dir: "data_store".to_string(),
            s3_bucket: "graze-turbo-01".to_string(),
            s3_region: "us-east-1".to_string(),
            rotation_minutes: 1,
            http_port: 8080,
            batch_size: 10,
            max_concurrent_requests: 100,
            cache_size_users: 20000,
            cache_size_posts: 20000,
            max_retries: 3,
            retry_base_delay: Duration::from_millis(100),
            statsd_host: None,
            statsd_port: None,
        }
    }
}

impl Settings {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        let mut settings = config::Config::builder()
            .add_source(config::Config::try_from(&Settings::default())?)
            .add_source(config::Environment::with_prefix("TURBO").separator("__"))
            .build()?;

        // Handle nested environment variables for special cases
        if let Ok(stream_name) = std::env::var("STREAM_NAME") {
            settings.set("stream_name", stream_name)?;
        }

        if let Ok(credential_secret) = std::env::var("TURBO_CREDENTIAL_SECRET") {
            settings.set("turbo_credential_secret", credential_secret)?;
        }

        let settings = settings.try_deserialize()?;

        // Validate required settings
        settings.validate()?;

        Ok(settings)
    }

    fn validate(&self) -> Result<()> {
        if self.stream_name.is_empty() {
            anyhow::bail!("STREAM_NAME environment variable is required");
        }

        if self.turbo_credential_secret.is_empty() {
            anyhow::bail!("TURBO_CREDENTIAL_SECRET environment variable is required");
        }

        if self.batch_size == 0 {
            anyhow::bail!("batch_size must be greater than 0");
        }

        if self.max_concurrent_requests == 0 {
            anyhow::bail!("max_concurrent_requests must be greater than 0");
        }

        Ok(())
    }
}

fn default_jetstream_hosts() -> Vec<String> {
    vec![
        "jetstream1.us-east.bsky.network".to_string(),
        "jetstream2.us-east.bsky.network".to_string(),
        "jetstream1.us-west.bsky.network".to_string(),
        "jetstream2.us-west.bsky.network".to_string(),
        "jetstream1.eu-west.bsky.network".to_string(),
        "jetstream2.eu-west.bsky.network".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_settings() {
        let settings = Settings::default();
        assert!(!settings.jetstream_hosts.is_empty());
        assert_eq!(settings.wanted_collections, "app.bsky.feed.post");
        assert_eq!(settings.batch_size, 10);
    }

    #[test]
    fn test_validation_missing_required_fields() {
        let mut settings = Settings::default();
        settings.stream_name = "".to_string();

        assert!(settings.validate().is_err());

        settings.stream_name = "test".to_string();
        settings.turbo_credential_secret = "".to_string();

        assert!(settings.validate().is_err());
    }
}
