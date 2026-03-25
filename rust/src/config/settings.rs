use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Settings {
    // Bluesky Authentication
    pub bluesky_handle: String,
    pub bluesky_app_password: String,

    // General Configuration
    pub stream_name: String,

    // Jetstream Configuration
    #[serde(default = "default_jetstream_hosts")]
    pub jetstream_hosts: Vec<String>,
    #[serde(default = "default_wanted_collections")]
    pub wanted_collections: String,

    // Redis Configuration
    pub redis_url: String,
    pub stream_name_redis: String,
    pub trim_maxlen: Option<usize>,

    // Storage Configuration
    pub db_dir: String,
    pub rotation_minutes: u64,

    // Cleanup Configuration
    pub max_db_size_mb: u64,
    pub db_retention_days: u32,
    pub cleanup_check_interval_minutes: u64,
    pub vacuum_min_bytes_freed: u64,
    pub vacuum_min_percent_freed: f64,
    pub cleanup_backoff_max_minutes: u64,
    pub cleanup_backoff_reset_count: u32,
    pub cleanup_chunk_size: u32,
    pub cleanup_chunk_delay_ms: u64,
    pub sqlite_cache_size_kib: u32,
    pub sqlite_mmap_size_mb: u64,
    pub sqlite_journal_size_limit_mb: u64,

    // HTTP Server Configuration
    pub http_port: u16,

    // Channel Configuration
    #[serde(default = "default_channel_capacity")]
    pub channel_capacity: usize,

    // Performance Configuration
    pub batch_size: usize,
    pub profile_batch_size: usize,
    pub post_batch_size: usize,
    pub profile_batch_wait_ms: u64,
    pub post_batch_wait_ms: u64,
    pub max_concurrent_requests: usize,
    pub cache_size_users: usize,
    pub cache_size_posts: usize,

    // Retry Configuration
    pub max_retries: u32,
    #[serde(skip)]
    pub retry_base_delay: Duration,

    // Metrics Configuration
    pub statsd_host: Option<String>,
    pub statsd_port: Option<u16>,

    // PostHog Configuration
    pub posthog_api_key: Option<String>,
    pub posthog_host: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            bluesky_handle: String::new(),
            bluesky_app_password: String::new(),
            stream_name: String::new(),
            jetstream_hosts: default_jetstream_hosts(),
            wanted_collections: default_wanted_collections(),
            redis_url: "redis://localhost:6379".to_string(),
            stream_name_redis: "hydrated_jetstream".to_string(),
            trim_maxlen: Some(100),
            db_dir: "data_store".to_string(),
            rotation_minutes: 1,
            // 8 GB RAM / 40 GB disk baseline:
            // tuned for higher throughput while still bounding growth.
            max_db_size_mb: 20 * 1024,
            db_retention_days: 3,
            cleanup_check_interval_minutes: 5,
            vacuum_min_bytes_freed: 100 * 1024 * 1024,
            vacuum_min_percent_freed: 10.0,
            cleanup_backoff_max_minutes: 30,
            cleanup_backoff_reset_count: 3,
            cleanup_chunk_size: 1000,
            cleanup_chunk_delay_ms: 50,
            sqlite_cache_size_kib: 64 * 1024,
            sqlite_mmap_size_mb: 256,
            sqlite_journal_size_limit_mb: 512,
            http_port: 8080,
            channel_capacity: default_channel_capacity(),
            batch_size: 10,
            profile_batch_size: 25,
            post_batch_size: 25,
            profile_batch_wait_ms: 150,
            post_batch_wait_ms: 300,
            max_concurrent_requests: 6,
            cache_size_users: 50_000,
            cache_size_posts: 40_000,
            max_retries: 3,
            retry_base_delay: Duration::from_millis(100),
            statsd_host: None,
            statsd_port: None,
            posthog_api_key: None,
            posthog_host: None,
        }
    }
}

impl Settings {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        let mut builder = config::Config::builder()
            .add_source(config::Config::try_from(&Settings::default())?)
            .add_source(config::Environment::with_prefix("TURBO").separator("__"));

        // Handle nested environment variables for special cases
        if let Ok(stream_name) = std::env::var("STREAM_NAME") {
            builder = builder.set_override("stream_name", stream_name)?;
        }

        if let Ok(handle) = std::env::var("BLUESKY_HANDLE") {
            builder = builder.set_override("bluesky_handle", handle)?;
        }

        if let Ok(password) = std::env::var("BLUESKY_APP_PASSWORD") {
            builder = builder.set_override("bluesky_app_password", password)?;
        }

        if let Ok(collections) = std::env::var("WANTED_COLLECTIONS") {
            builder = builder.set_override("wanted_collections", collections)?;
        }

        if let Ok(hosts) = std::env::var("JETSTREAM_HOSTS") {
            let hosts: Vec<String> = serde_json::from_str(&hosts)?;
            builder = builder.set_override("jetstream_hosts", hosts)?;
        }

        // Cleanup Configuration
        if let Ok(max_db_size_mb) = std::env::var("MAX_DB_SIZE_MB") {
            builder = builder.set_override("max_db_size_mb", max_db_size_mb)?;
        }

        if let Ok(db_retention_days) = std::env::var("DB_RETENTION_DAYS") {
            builder = builder.set_override("db_retention_days", db_retention_days)?;
        }

        if let Ok(cleanup_check_interval) = std::env::var("CLEANUP_CHECK_INTERVAL_MINUTES") {
            builder =
                builder.set_override("cleanup_check_interval_minutes", cleanup_check_interval)?;
        }

        if let Ok(vacuum_min_bytes) = std::env::var("VACUUM_MIN_BYTES_FREED") {
            builder = builder.set_override("vacuum_min_bytes_freed", vacuum_min_bytes)?;
        }

        if let Ok(vacuum_min_percent) = std::env::var("VACUUM_MIN_PERCENT_FREED") {
            builder = builder.set_override("vacuum_min_percent_freed", vacuum_min_percent)?;
        }

        if let Ok(backoff_max) = std::env::var("CLEANUP_BACKOFF_MAX_MINUTES") {
            builder = builder.set_override("cleanup_backoff_max_minutes", backoff_max)?;
        }

        if let Ok(reset_count) = std::env::var("CLEANUP_BACKOFF_RESET_COUNT") {
            builder = builder.set_override("cleanup_backoff_reset_count", reset_count)?;
        }

        if let Ok(chunk_size) = std::env::var("CLEANUP_CHUNK_SIZE") {
            builder = builder.set_override("cleanup_chunk_size", chunk_size)?;
        }

        if let Ok(chunk_delay) = std::env::var("CLEANUP_CHUNK_DELAY_MS") {
            builder = builder.set_override("cleanup_chunk_delay_ms", chunk_delay)?;
        }

        if let Ok(sqlite_cache_size_kib) = std::env::var("SQLITE_CACHE_SIZE_KIB") {
            builder = builder.set_override("sqlite_cache_size_kib", sqlite_cache_size_kib)?;
        }

        if let Ok(sqlite_mmap_size_mb) = std::env::var("SQLITE_MMAP_SIZE_MB") {
            builder = builder.set_override("sqlite_mmap_size_mb", sqlite_mmap_size_mb)?;
        }

        if let Ok(sqlite_journal_size_limit_mb) = std::env::var("SQLITE_JOURNAL_SIZE_LIMIT_MB") {
            builder = builder
                .set_override("sqlite_journal_size_limit_mb", sqlite_journal_size_limit_mb)?;
        }

        // Resource knobs with explicit env names for operability in .env files.
        if let Ok(max_concurrent_requests) = std::env::var("MAX_CONCURRENT_REQUESTS") {
            builder = builder.set_override("max_concurrent_requests", max_concurrent_requests)?;
        }

        if let Ok(cache_size_users) = std::env::var("CACHE_SIZE_USERS") {
            builder = builder.set_override("cache_size_users", cache_size_users)?;
        }

        if let Ok(cache_size_posts) = std::env::var("CACHE_SIZE_POSTS") {
            builder = builder.set_override("cache_size_posts", cache_size_posts)?;
        }

        if let Ok(channel_capacity) = std::env::var("CHANNEL_CAPACITY") {
            builder = builder.set_override("channel_capacity", channel_capacity)?;
        }

        if let Ok(trim_maxlen) = std::env::var("TRIM_MAXLEN") {
            builder = builder.set_override("trim_maxlen", trim_maxlen)?;
        }

        if let Ok(posthog_api_key) = std::env::var("POSTHOG_API_KEY") {
            builder = builder.set_override("posthog_api_key", posthog_api_key)?;
        }

        if let Ok(posthog_host) = std::env::var("POSTHOG_HOST") {
            builder = builder.set_override("posthog_host", posthog_host)?;
        }

        let settings = builder.build()?;
        let mut settings: Settings = settings.try_deserialize()?;
        settings.posthog_api_key = normalize_optional_setting(settings.posthog_api_key);
        settings.posthog_host = normalize_optional_setting(settings.posthog_host);

        // Validate required settings
        settings.validate()?;

        Ok(settings)
    }

    fn validate(&self) -> Result<()> {
        if self.stream_name.is_empty() {
            anyhow::bail!(
                "STREAM_NAME environment variable is required\n\n\
                To set up:\n\
                1. Copy .env.example to .env\n\
                2. Set STREAM_NAME in .env (e.g., STREAM_NAME=hydrated_jetstream)"
            );
        }

        if self.bluesky_handle.is_empty() {
            anyhow::bail!(
                "BLUESKY_HANDLE environment variable is required\n\n\
                To set up:\n\
                1. Copy .env.example to .env\n\
                2. Set BLUESKY_HANDLE in .env (e.g., BLUESKY_HANDLE=yourname.bsky.social)\n\n\
                Get your handle from your Bluesky profile."
            );
        }

        if self.bluesky_app_password.is_empty() {
            anyhow::bail!(
                "BLUESKY_APP_PASSWORD environment variable is required\n\n\
                To set up:\n\
                1. Go to https://bsky.app/settings/app-passwords\n\
                2. Create a new app password\n\
                3. Copy .env.example to .env\n\
                4. Set BLUESKY_APP_PASSWORD in .env"
            );
        }

        if self.batch_size == 0 {
            anyhow::bail!("batch_size must be greater than 0");
        }

        if self.max_concurrent_requests == 0 {
            anyhow::bail!("max_concurrent_requests must be greater than 0");
        }

        if self.cache_size_users == 0 || self.cache_size_posts == 0 {
            anyhow::bail!("cache_size_users and cache_size_posts must be greater than 0");
        }

        if self.max_db_size_mb == 0 {
            anyhow::bail!("max_db_size_mb must be greater than 0");
        }

        if self.sqlite_cache_size_kib == 0 {
            anyhow::bail!("sqlite_cache_size_kib must be greater than 0");
        }

        if self.sqlite_mmap_size_mb == 0 {
            anyhow::bail!("sqlite_mmap_size_mb must be greater than 0");
        }

        if self.sqlite_journal_size_limit_mb == 0 {
            anyhow::bail!("sqlite_journal_size_limit_mb must be greater than 0");
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

fn default_channel_capacity() -> usize {
    10_000
}

fn default_wanted_collections() -> String {
    "app.bsky.feed.post".to_string()
}

fn normalize_optional_setting(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
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
        assert_eq!(settings.max_db_size_mb, 20 * 1024);
        assert_eq!(settings.max_concurrent_requests, 6);
        assert_eq!(settings.cache_size_users, 50_000);
        assert_eq!(settings.cache_size_posts, 40_000);
        assert_eq!(settings.sqlite_cache_size_kib, 64 * 1024);
        assert_eq!(settings.sqlite_mmap_size_mb, 256);
        assert_eq!(settings.sqlite_journal_size_limit_mb, 512);
    }

    #[test]
    fn test_validation_missing_required_fields() {
        let mut settings = Settings::default();
        settings.stream_name = "".to_string();

        assert!(settings.validate().is_err());

        settings.stream_name = "test".to_string();
        settings.bluesky_handle = "".to_string();

        assert!(settings.validate().is_err());

        settings.bluesky_handle = "test.bsky.social".to_string();
        settings.bluesky_app_password = "".to_string();

        assert!(settings.validate().is_err());
    }

    #[test]
    fn test_normalize_optional_setting() {
        assert_eq!(normalize_optional_setting(None), None);
        assert_eq!(normalize_optional_setting(Some("".to_string())), None);
        assert_eq!(normalize_optional_setting(Some("   ".to_string())), None);
        assert_eq!(
            normalize_optional_setting(Some("  https://us.i.posthog.com  ".to_string())),
            Some("https://us.i.posthog.com".to_string())
        );
    }
}
