use crate::client::{
    DEFAULT_POST_COORDINATION_KEY_CAPACITY, DEFAULT_POST_COORDINATION_WAITER_CAPACITY,
    DEFAULT_PROFILE_COORDINATION_KEY_CAPACITY, DEFAULT_PROFILE_COORDINATION_WAITER_CAPACITY,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

use crate::hydration::HydrationExecutionMode;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Settings {
    // Bluesky Authentication
    pub bluesky_handle: String,
    pub bluesky_app_password: String,
    pub bluesky_api_url: String,

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
    pub vacuum_freelist_ratio: f64,
    /// First hour of the UTC window in which pending VACUUMs are allowed to run.
    pub vacuum_window_start_hour: u32,
    /// Last (exclusive) hour of the UTC window in which pending VACUUMs run.
    pub vacuum_window_end_hour: u32,
    /// Maximum hours a pending VACUUM may be deferred past the window before
    /// it runs regardless (bounds the worst case).
    pub vacuum_max_defer_hours: u64,
    pub cleanup_backoff_max_minutes: u64,
    pub cleanup_backoff_reset_count: u32,
    pub cleanup_chunk_size: u32,
    pub cleanup_chunk_delay_ms: u64,
    pub sqlite_cache_size_kib: u32,
    pub sqlite_mmap_size_mb: u64,
    pub sqlite_journal_size_limit_mb: u64,
    /// Maximum concurrent SQLite connections. Each connection owns a page cache.
    pub sqlite_max_connections: u32,
    pub sqlite_schema_maintenance_busy_timeout_secs: u64,

    // HTTP Server Configuration
    pub http_port: u16,

    // Channel Configuration
    #[serde(default = "default_channel_capacity")]
    pub channel_capacity: usize,
    #[serde(default = "default_monitor_broadcast_capacity")]
    pub monitor_broadcast_capacity: usize,

    // Pipeline Progress and Recovery Configuration
    pub jetstream_data_idle_timeout_secs: u64,
    pub jetstream_connect_timeout_secs: u64,
    pub jetstream_cursor_overlap_secs: u64,
    pub jetstream_endpoint_backoff_min_secs: u64,
    pub jetstream_endpoint_backoff_max_secs: u64,
    pub jetstream_committed_lag_threshold_secs: u64,
    pub jetstream_live_stability_observations: u32,
    pub jetstream_recovery_deadlines_enabled: bool,
    pub jetstream_cursor_replay_enabled: bool,
    pub batch_execution_timeout_secs: u64,
    pub pipeline_startup_grace_secs: u64,
    pub readiness_recovery_successes: u32,
    pub pipeline_progress_readiness_enabled: bool,
    pub pipeline_deadlines_enabled: bool,

    // Performance Configuration
    pub batch_size: usize,
    pub profile_batch_size: usize,
    pub post_batch_size: usize,
    pub profile_batch_wait_ms: u64,
    pub post_batch_wait_ms: u64,
    pub profile_coordination_key_capacity: usize,
    pub profile_coordination_waiter_capacity: usize,
    pub post_coordination_key_capacity: usize,
    pub post_coordination_waiter_capacity: usize,
    pub max_concurrent_requests: usize,
    /// Temporary one-release-cycle switch for overlapping independent hydration branches.
    pub hydration_execution_mode: HydrationExecutionMode,
    pub cache_size_users: usize,
    pub cache_size_posts: usize,
    pub user_cache_limit_mb: u64,
    pub post_cache_limit_mb: u64,
    /// Maximum temporarily unavailable referenced-post outcomes retained in memory.
    pub negative_post_cache_capacity: usize,
    pub negative_post_cache_limit_mb: u64,
    /// Expiry for temporarily unavailable referenced-post outcomes.
    #[serde(skip)]
    pub negative_post_cache_ttl: Duration,

    // Runtime memory safety
    pub memory_recovery_mb: u64,
    pub memory_soft_pressure_mb: u64,
    pub memory_emergency_mb: u64,
    pub memory_external_hard_limit_mb: u64,
    pub memory_host_total_mb: u64,
    pub memory_required_host_headroom_mb: u64,
    pub memory_pressure_confirmation_secs: u64,
    pub memory_recovery_confirmation_secs: u64,
    pub memory_sample_interval_secs: u64,
    pub memory_sample_capacity: usize,
    pub in_flight_payload_limit_mb: u64,
    pub max_ingress_event_bytes: usize,
    pub memory_pressure_actions_enabled: bool,
    pub memory_emergency_exit_enabled: bool,

    // Retry Configuration
    pub max_retries: u32,
    #[serde(skip)]
    pub retry_base_delay: Duration,
    #[serde(skip)]
    pub retry_max_delay: Duration,

    // Persistent Bluesky Failure Containment
    #[serde(skip)]
    pub recovery_min_delay: Duration,
    #[serde(skip)]
    pub recovery_max_delay: Duration,
    pub recovery_persistence_threshold: u32,
    pub isolation_request_budget: u32,

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
            bluesky_api_url: "https://bsky.social/xrpc".to_string(),
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
            // Freelist pages (freed by DELETE but not returned to the OS)
            // above 10% of the database trigger a proactive VACUUM.
            vacuum_freelist_ratio: 0.10,
            vacuum_window_start_hour: 3,
            vacuum_window_end_hour: 5,
            vacuum_max_defer_hours: 6,
            cleanup_backoff_max_minutes: 30,
            cleanup_backoff_reset_count: 3,
            cleanup_chunk_size: 1000,
            cleanup_chunk_delay_ms: 50,
            sqlite_cache_size_kib: 64 * 1024,
            sqlite_mmap_size_mb: 256,
            sqlite_journal_size_limit_mb: 512,
            sqlite_max_connections: 4,
            sqlite_schema_maintenance_busy_timeout_secs: 30,
            http_port: 8080,
            channel_capacity: default_channel_capacity(),
            monitor_broadcast_capacity: default_monitor_broadcast_capacity(),
            jetstream_data_idle_timeout_secs: 30,
            jetstream_connect_timeout_secs: 10,
            jetstream_cursor_overlap_secs: 10,
            jetstream_endpoint_backoff_min_secs: 1,
            jetstream_endpoint_backoff_max_secs: 30,
            jetstream_committed_lag_threshold_secs: 30,
            jetstream_live_stability_observations: 3,
            jetstream_recovery_deadlines_enabled: true,
            jetstream_cursor_replay_enabled: true,
            batch_execution_timeout_secs: 60,
            pipeline_startup_grace_secs: 300,
            readiness_recovery_successes: 3,
            pipeline_progress_readiness_enabled: false,
            pipeline_deadlines_enabled: false,
            batch_size: 10,
            profile_batch_size: 25,
            post_batch_size: 25,
            profile_batch_wait_ms: 150,
            post_batch_wait_ms: 300,
            profile_coordination_key_capacity: DEFAULT_PROFILE_COORDINATION_KEY_CAPACITY,
            profile_coordination_waiter_capacity: DEFAULT_PROFILE_COORDINATION_WAITER_CAPACITY,
            post_coordination_key_capacity: DEFAULT_POST_COORDINATION_KEY_CAPACITY,
            post_coordination_waiter_capacity: DEFAULT_POST_COORDINATION_WAITER_CAPACITY,
            max_concurrent_requests: 9,
            hydration_execution_mode: HydrationExecutionMode::Parallel,
            cache_size_users: 50_000,
            cache_size_posts: 40_000,
            user_cache_limit_mb: 512,
            post_cache_limit_mb: 1024,
            // Sized for roughly half of the positive post cache's expected unique
            // reference volume while bounding outage-related memory growth.
            negative_post_cache_capacity: 20_000,
            negative_post_cache_limit_mb: 128,
            negative_post_cache_ttl: Duration::from_secs(5 * 60),
            memory_recovery_mb: 3072,
            memory_soft_pressure_mb: 3584,
            memory_emergency_mb: 4608,
            memory_external_hard_limit_mb: 5120,
            memory_host_total_mb: 8192,
            memory_required_host_headroom_mb: 2048,
            memory_pressure_confirmation_secs: 30,
            memory_recovery_confirmation_secs: 60,
            memory_sample_interval_secs: 5,
            memory_sample_capacity: 720,
            in_flight_payload_limit_mb: 256,
            max_ingress_event_bytes: 256 * 1024,
            memory_pressure_actions_enabled: false,
            memory_emergency_exit_enabled: false,
            max_retries: 3,
            retry_base_delay: Duration::from_millis(100),
            retry_max_delay: Duration::from_secs(5),
            recovery_min_delay: Duration::from_secs(5),
            recovery_max_delay: Duration::from_secs(5 * 60),
            recovery_persistence_threshold: 3,
            isolation_request_budget: 8,
            statsd_host: None,
            statsd_port: None,
            posthog_api_key: None,
            posthog_host: None,
        }
    }
}

impl Settings {
    pub fn from_env() -> Result<Self> {
        let settings = Self::load_from_env()?;
        settings.validate()?;
        Ok(settings)
    }

    /// Loads only the settings needed for offline SQLite schema maintenance.
    pub fn from_env_for_schema_maintenance() -> Result<Self> {
        let settings = Self::load_from_env()?;
        settings.validate_schema_maintenance()?;
        Ok(settings)
    }

    pub fn database_path(&self) -> PathBuf {
        PathBuf::from(&self.db_dir).join("jetstream.db")
    }

    fn load_from_env() -> Result<Self> {
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

        if let Ok(api_url) = std::env::var("BLUESKY_API_URL") {
            builder = builder.set_override("bluesky_api_url", api_url)?;
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

        if let Ok(vacuum_freelist_ratio) = std::env::var("VACUUM_FREELIST_RATIO") {
            builder = builder.set_override("vacuum_freelist_ratio", vacuum_freelist_ratio)?;
        }

        if let Ok(window_start) = std::env::var("VACUUM_WINDOW_START_HOUR") {
            builder = builder.set_override("vacuum_window_start_hour", window_start)?;
        }

        if let Ok(window_end) = std::env::var("VACUUM_WINDOW_END_HOUR") {
            builder = builder.set_override("vacuum_window_end_hour", window_end)?;
        }

        if let Ok(max_defer) = std::env::var("VACUUM_MAX_DEFER_HOURS") {
            builder = builder.set_override("vacuum_max_defer_hours", max_defer)?;
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
        if let Ok(value) = std::env::var("SQLITE_MAX_CONNECTIONS") {
            builder = builder.set_override("sqlite_max_connections", value)?;
        }
        if let Ok(value) = std::env::var("SQLITE_SCHEMA_MAINTENANCE_BUSY_TIMEOUT_SECS") {
            builder = builder.set_override("sqlite_schema_maintenance_busy_timeout_secs", value)?;
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
        if let Ok(value) = std::env::var("USER_CACHE_LIMIT_MB") {
            builder = builder.set_override("user_cache_limit_mb", value)?;
        }
        if let Ok(value) = std::env::var("POST_CACHE_LIMIT_MB") {
            builder = builder.set_override("post_cache_limit_mb", value)?;
        }
        if let Ok(value) = std::env::var("NEGATIVE_POST_CACHE_LIMIT_MB") {
            builder = builder.set_override("negative_post_cache_limit_mb", value)?;
        }

        for (environment, setting) in [
            ("MEMORY_RECOVERY_MB", "memory_recovery_mb"),
            ("MEMORY_SOFT_PRESSURE_MB", "memory_soft_pressure_mb"),
            ("MEMORY_EMERGENCY_MB", "memory_emergency_mb"),
            (
                "MEMORY_EXTERNAL_HARD_LIMIT_MB",
                "memory_external_hard_limit_mb",
            ),
            ("MEMORY_HOST_TOTAL_MB", "memory_host_total_mb"),
            (
                "MEMORY_REQUIRED_HOST_HEADROOM_MB",
                "memory_required_host_headroom_mb",
            ),
            (
                "MEMORY_PRESSURE_CONFIRMATION_SECS",
                "memory_pressure_confirmation_secs",
            ),
            (
                "MEMORY_RECOVERY_CONFIRMATION_SECS",
                "memory_recovery_confirmation_secs",
            ),
            ("MEMORY_SAMPLE_INTERVAL_SECS", "memory_sample_interval_secs"),
            ("MEMORY_SAMPLE_CAPACITY", "memory_sample_capacity"),
            ("IN_FLIGHT_PAYLOAD_LIMIT_MB", "in_flight_payload_limit_mb"),
            ("MAX_INGRESS_EVENT_BYTES", "max_ingress_event_bytes"),
            (
                "MEMORY_PRESSURE_ACTIONS_ENABLED",
                "memory_pressure_actions_enabled",
            ),
            (
                "MEMORY_EMERGENCY_EXIT_ENABLED",
                "memory_emergency_exit_enabled",
            ),
        ] {
            if let Ok(value) = std::env::var(environment) {
                builder = builder.set_override(setting, value)?;
            }
        }

        if let Ok(channel_capacity) = std::env::var("CHANNEL_CAPACITY") {
            builder = builder.set_override("channel_capacity", channel_capacity)?;
        }

        if let Ok(monitor_broadcast_capacity) = std::env::var("MONITOR_BROADCAST_CAPACITY") {
            builder =
                builder.set_override("monitor_broadcast_capacity", monitor_broadcast_capacity)?;
        }

        if let Ok(value) = std::env::var("JETSTREAM_DATA_IDLE_TIMEOUT_SECS") {
            builder = builder.set_override("jetstream_data_idle_timeout_secs", value)?;
        }
        if let Ok(value) = std::env::var("JETSTREAM_CONNECT_TIMEOUT_SECS") {
            builder = builder.set_override("jetstream_connect_timeout_secs", value)?;
        }
        if let Ok(value) = std::env::var("JETSTREAM_CURSOR_OVERLAP_SECS") {
            builder = builder.set_override("jetstream_cursor_overlap_secs", value)?;
        }
        if let Ok(value) = std::env::var("JETSTREAM_ENDPOINT_BACKOFF_MIN_SECS") {
            builder = builder.set_override("jetstream_endpoint_backoff_min_secs", value)?;
        }
        if let Ok(value) = std::env::var("JETSTREAM_ENDPOINT_BACKOFF_MAX_SECS") {
            builder = builder.set_override("jetstream_endpoint_backoff_max_secs", value)?;
        }
        if let Ok(value) = std::env::var("JETSTREAM_COMMITTED_LAG_THRESHOLD_SECS") {
            builder = builder.set_override("jetstream_committed_lag_threshold_secs", value)?;
        }
        if let Ok(value) = std::env::var("JETSTREAM_LIVE_STABILITY_OBSERVATIONS") {
            builder = builder.set_override("jetstream_live_stability_observations", value)?;
        }
        if let Ok(value) = std::env::var("JETSTREAM_RECOVERY_DEADLINES_ENABLED") {
            builder = builder.set_override("jetstream_recovery_deadlines_enabled", value)?;
        }
        if let Ok(value) = std::env::var("JETSTREAM_CURSOR_REPLAY_ENABLED") {
            builder = builder.set_override("jetstream_cursor_replay_enabled", value)?;
        }
        if let Ok(value) = std::env::var("BATCH_EXECUTION_TIMEOUT_SECS") {
            builder = builder.set_override("batch_execution_timeout_secs", value)?;
        }
        if let Ok(value) = std::env::var("PIPELINE_STARTUP_GRACE_SECS") {
            builder = builder.set_override("pipeline_startup_grace_secs", value)?;
        }
        if let Ok(value) = std::env::var("READINESS_RECOVERY_SUCCESSES") {
            builder = builder.set_override("readiness_recovery_successes", value)?;
        }
        if let Ok(value) = std::env::var("PIPELINE_PROGRESS_READINESS_ENABLED") {
            builder = builder.set_override("pipeline_progress_readiness_enabled", value)?;
        }
        if let Ok(value) = std::env::var("PIPELINE_DEADLINES_ENABLED") {
            builder = builder.set_override("pipeline_deadlines_enabled", value)?;
        }

        if let Ok(value) = std::env::var("BLUESKY_MAX_RETRIES") {
            builder = builder.set_override("max_retries", value)?;
        }
        if let Ok(value) = std::env::var("BLUESKY_RECOVERY_PERSISTENCE_THRESHOLD") {
            builder = builder.set_override("recovery_persistence_threshold", value)?;
        }
        if let Ok(value) = std::env::var("BLUESKY_ISOLATION_REQUEST_BUDGET") {
            builder = builder.set_override("isolation_request_budget", value)?;
        }
        if let Ok(value) = std::env::var("BLUESKY_NEGATIVE_POST_CACHE_CAPACITY") {
            builder = builder.set_override("negative_post_cache_capacity", value)?;
        }
        if let Ok(value) = std::env::var("BLUESKY_PROFILE_COORDINATION_KEY_CAPACITY") {
            builder = builder.set_override("profile_coordination_key_capacity", value)?;
        }
        if let Ok(value) = std::env::var("BLUESKY_PROFILE_COORDINATION_WAITER_CAPACITY") {
            builder = builder.set_override("profile_coordination_waiter_capacity", value)?;
        }
        if let Ok(value) = std::env::var("BLUESKY_POST_COORDINATION_KEY_CAPACITY") {
            builder = builder.set_override("post_coordination_key_capacity", value)?;
        }
        if let Ok(value) = std::env::var("BLUESKY_POST_COORDINATION_WAITER_CAPACITY") {
            builder = builder.set_override("post_coordination_waiter_capacity", value)?;
        }
        if let Ok(value) = std::env::var("HYDRATION_EXECUTION_MODE") {
            builder = builder.set_override("hydration_execution_mode", value)?;
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
        settings.retry_base_delay = duration_from_env_ms(
            "BLUESKY_RETRY_BASE_DELAY_MS",
            Settings::default().retry_base_delay,
        )?;
        settings.retry_max_delay = duration_from_env_ms(
            "BLUESKY_RETRY_MAX_DELAY_MS",
            Settings::default().retry_max_delay,
        )?;
        settings.recovery_min_delay = duration_from_env_ms(
            "BLUESKY_RECOVERY_MIN_DELAY_MS",
            Settings::default().recovery_min_delay,
        )?;
        settings.recovery_max_delay = duration_from_env_ms(
            "BLUESKY_RECOVERY_MAX_DELAY_MS",
            Settings::default().recovery_max_delay,
        )?;
        settings.negative_post_cache_ttl = duration_from_env_ms(
            "BLUESKY_NEGATIVE_POST_CACHE_TTL_MS",
            Settings::default().negative_post_cache_ttl,
        )?;
        settings.posthog_api_key = normalize_optional_setting(settings.posthog_api_key);
        settings.posthog_host = normalize_optional_setting(settings.posthog_host);

        Ok(settings)
    }

    fn validate_schema_maintenance(&self) -> Result<()> {
        if self.db_dir.trim().is_empty() {
            anyhow::bail!("db_dir must not be empty");
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
        if self.sqlite_schema_maintenance_busy_timeout_secs == 0 {
            anyhow::bail!("sqlite_schema_maintenance_busy_timeout_secs must be greater than 0");
        }
        Ok(())
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

        if self.bluesky_api_url.trim().is_empty() {
            anyhow::bail!("BLUESKY_API_URL must not be empty");
        }

        if self.batch_size == 0 {
            anyhow::bail!("batch_size must be greater than 0");
        }

        if self.max_concurrent_requests == 0 {
            anyhow::bail!("max_concurrent_requests must be greater than 0");
        }

        if self.channel_capacity == 0 {
            anyhow::bail!("channel_capacity must be greater than 0");
        }

        if self.monitor_broadcast_capacity == 0 {
            anyhow::bail!("monitor_broadcast_capacity must be greater than 0");
        }

        if !(1..=3_600).contains(&self.jetstream_data_idle_timeout_secs) {
            anyhow::bail!("jetstream_data_idle_timeout_secs must be between 1 and 3600");
        }
        if !(1..=300).contains(&self.jetstream_connect_timeout_secs) {
            anyhow::bail!("jetstream_connect_timeout_secs must be between 1 and 300");
        }
        if self.jetstream_cursor_overlap_secs > 86_400 {
            anyhow::bail!("jetstream_cursor_overlap_secs must be at most 86400");
        }
        if !(1..=3_600).contains(&self.jetstream_endpoint_backoff_min_secs) {
            anyhow::bail!("jetstream_endpoint_backoff_min_secs must be between 1 and 3600");
        }
        if !(1..=3_600).contains(&self.jetstream_endpoint_backoff_max_secs) {
            anyhow::bail!("jetstream_endpoint_backoff_max_secs must be between 1 and 3600");
        }
        if self.jetstream_endpoint_backoff_min_secs > self.jetstream_endpoint_backoff_max_secs {
            anyhow::bail!(
                "jetstream_endpoint_backoff_min_secs must not exceed jetstream_endpoint_backoff_max_secs"
            );
        }
        if !(1..=86_400).contains(&self.jetstream_committed_lag_threshold_secs) {
            anyhow::bail!("jetstream_committed_lag_threshold_secs must be between 1 and 86400");
        }
        if !(1..=1_000).contains(&self.jetstream_live_stability_observations) {
            anyhow::bail!("jetstream_live_stability_observations must be between 1 and 1000");
        }
        if self.batch_execution_timeout_secs == 0 {
            anyhow::bail!("batch_execution_timeout_secs must be greater than 0");
        }
        if self.pipeline_startup_grace_secs == 0 {
            anyhow::bail!("pipeline_startup_grace_secs must be greater than 0");
        }
        if self.readiness_recovery_successes == 0 {
            anyhow::bail!("readiness_recovery_successes must be greater than 0");
        }

        if self.retry_base_delay.is_zero() {
            anyhow::bail!("BLUESKY_RETRY_BASE_DELAY_MS must be greater than 0");
        }
        if self.retry_base_delay > self.retry_max_delay {
            anyhow::bail!("BLUESKY_RETRY_BASE_DELAY_MS must not exceed BLUESKY_RETRY_MAX_DELAY_MS");
        }
        if self.recovery_min_delay.is_zero() {
            anyhow::bail!("BLUESKY_RECOVERY_MIN_DELAY_MS must be greater than 0");
        }
        if self.recovery_min_delay > self.recovery_max_delay {
            anyhow::bail!(
                "BLUESKY_RECOVERY_MIN_DELAY_MS must not exceed BLUESKY_RECOVERY_MAX_DELAY_MS"
            );
        }
        if self.recovery_persistence_threshold == 0 {
            anyhow::bail!("BLUESKY_RECOVERY_PERSISTENCE_THRESHOLD must be greater than 0");
        }
        if self.isolation_request_budget == 0 {
            anyhow::bail!("BLUESKY_ISOLATION_REQUEST_BUDGET must be greater than 0");
        }

        if self.cache_size_users == 0 || self.cache_size_posts == 0 {
            anyhow::bail!("cache_size_users and cache_size_posts must be greater than 0");
        }
        if self.user_cache_limit_mb == 0
            || self.post_cache_limit_mb == 0
            || self.negative_post_cache_limit_mb == 0
        {
            anyhow::bail!("hydration cache byte limits must be greater than 0");
        }
        if self.profile_coordination_key_capacity < self.profile_batch_size {
            anyhow::bail!(
                "BLUESKY_PROFILE_COORDINATION_KEY_CAPACITY must be at least profile_batch_size ({})",
                self.profile_batch_size
            );
        }
        if self.post_coordination_key_capacity < self.post_batch_size {
            anyhow::bail!(
                "BLUESKY_POST_COORDINATION_KEY_CAPACITY must be at least post_batch_size ({})",
                self.post_batch_size
            );
        }
        if self.profile_coordination_waiter_capacity < self.profile_batch_size {
            anyhow::bail!(
                "BLUESKY_PROFILE_COORDINATION_WAITER_CAPACITY must be at least profile_batch_size ({})",
                self.profile_batch_size
            );
        }
        if self.post_coordination_waiter_capacity < self.post_batch_size {
            anyhow::bail!(
                "BLUESKY_POST_COORDINATION_WAITER_CAPACITY must be at least post_batch_size ({})",
                self.post_batch_size
            );
        }
        if self.negative_post_cache_capacity == 0 {
            anyhow::bail!("BLUESKY_NEGATIVE_POST_CACHE_CAPACITY must be greater than 0");
        }
        if self.negative_post_cache_ttl.is_zero() {
            anyhow::bail!("BLUESKY_NEGATIVE_POST_CACHE_TTL_MS must be greater than 0");
        }

        if self.max_db_size_mb == 0 {
            anyhow::bail!("max_db_size_mb must be greater than 0");
        }

        if !(0.0..=1.0).contains(&self.vacuum_freelist_ratio) {
            anyhow::bail!("vacuum_freelist_ratio must be between 0 and 1");
        }

        if self.vacuum_window_start_hour > 23 || self.vacuum_window_end_hour > 23 {
            anyhow::bail!(
                "vacuum_window_start_hour and vacuum_window_end_hour must be between 0 and 23"
            );
        }

        if self.vacuum_max_defer_hours == 0 {
            anyhow::bail!("vacuum_max_defer_hours must be greater than 0");
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

        if self.sqlite_max_connections == 0 {
            anyhow::bail!("sqlite_max_connections must be greater than 0");
        }

        self.memory_envelope().validate()?;
        if self.memory_sample_interval_secs == 0 || self.memory_sample_capacity == 0 {
            anyhow::bail!("memory sampling interval and capacity must be greater than 0");
        }
        if self.in_flight_payload_limit_mb == 0 || self.max_ingress_event_bytes == 0 {
            anyhow::bail!("in-flight payload and ingress event byte limits must be greater than 0");
        }
        let required_in_flight_bytes = u64::try_from(self.max_concurrent_requests)
            .unwrap_or(u64::MAX)
            .saturating_mul(25)
            .saturating_mul(u64::try_from(self.max_ingress_event_bytes).unwrap_or(u64::MAX));
        if self.in_flight_payload_limit_mb.saturating_mul(1024 * 1024) < required_in_flight_bytes {
            anyhow::bail!(
                "in_flight_payload_limit_mb must cover max_concurrent_requests * 25 records * max_ingress_event_bytes"
            );
        }
        if self.conservative_memory_working_set_bytes() > self.memory_envelope().recovery_bytes {
            anyhow::bail!(
                "bounded cache, queue, payload, and SQLite working set must fit below memory_recovery_mb"
            );
        }

        if self.sqlite_schema_maintenance_busy_timeout_secs == 0 {
            anyhow::bail!("sqlite_schema_maintenance_busy_timeout_secs must be greater than 0");
        }

        Ok(())
    }

    pub fn memory_envelope(&self) -> crate::turbocharger::runtime_memory::MemoryEnvelope {
        const MIB: u64 = 1024 * 1024;
        crate::turbocharger::runtime_memory::MemoryEnvelope {
            recovery_bytes: self.memory_recovery_mb.saturating_mul(MIB),
            soft_pressure_bytes: self.memory_soft_pressure_mb.saturating_mul(MIB),
            emergency_bytes: self.memory_emergency_mb.saturating_mul(MIB),
            external_hard_limit_bytes: self.memory_external_hard_limit_mb.saturating_mul(MIB),
            host_memory_bytes: self.memory_host_total_mb.saturating_mul(MIB),
            required_host_headroom_bytes: self.memory_required_host_headroom_mb.saturating_mul(MIB),
            pressure_confirmation_seconds: self.memory_pressure_confirmation_secs,
            recovery_confirmation_seconds: self.memory_recovery_confirmation_secs,
        }
    }

    pub fn conservative_memory_working_set_bytes(&self) -> u64 {
        const MIB: u64 = 1024 * 1024;
        const MAX_MONITOR_RECORD_BYTES: u64 = 2 * MIB;
        const COORDINATION_IDENTIFIER_BYTES: u64 = 8 * 1024;
        const COORDINATION_KEY_METADATA_BYTES: u64 = 192;
        const COORDINATION_WAITER_BYTES: u64 = 128;
        const MEMORY_SAMPLE_BYTES: u64 = 4 * 1024;
        let mib_bounded = self
            .user_cache_limit_mb
            .saturating_add(self.post_cache_limit_mb)
            .saturating_add(self.negative_post_cache_limit_mb)
            .saturating_add(self.in_flight_payload_limit_mb)
            .saturating_add(self.sqlite_mmap_size_mb)
            .saturating_mul(MIB);
        let coordination_bytes = [
            (
                self.profile_coordination_key_capacity,
                self.profile_coordination_waiter_capacity,
            ),
            (
                self.post_coordination_key_capacity,
                self.post_coordination_waiter_capacity,
            ),
        ]
        .into_iter()
        .fold(0_u64, |total, (keys, waiters)| {
            total
                .saturating_add(u64::try_from(keys).unwrap_or(u64::MAX).saturating_mul(
                    COORDINATION_IDENTIFIER_BYTES.saturating_add(COORDINATION_KEY_METADATA_BYTES),
                ))
                .saturating_add(
                    u64::try_from(waiters)
                        .unwrap_or(u64::MAX)
                        .saturating_mul(COORDINATION_WAITER_BYTES),
                )
        });
        mib_bounded
            .saturating_add(
                u64::from(self.sqlite_max_connections)
                    .saturating_mul(u64::from(self.sqlite_cache_size_kib))
                    .saturating_mul(1024),
            )
            .saturating_add(
                u64::try_from(self.channel_capacity)
                    .unwrap_or(u64::MAX)
                    .saturating_mul(
                        u64::try_from(self.max_ingress_event_bytes).unwrap_or(u64::MAX),
                    ),
            )
            .saturating_add(
                u64::try_from(self.monitor_broadcast_capacity)
                    .unwrap_or(u64::MAX)
                    .saturating_mul(MAX_MONITOR_RECORD_BYTES),
            )
            .saturating_add(coordination_bytes)
            .saturating_add(
                u64::try_from(self.memory_sample_capacity)
                    .unwrap_or(u64::MAX)
                    .saturating_mul(MEMORY_SAMPLE_BYTES),
            )
    }
}

fn default_jetstream_hosts() -> Vec<String> {
    vec![
        "jetstream.us-west.bsky.network".to_string(),
        "jetstream.us-east.bsky.network".to_string(),
    ]
}

fn default_channel_capacity() -> usize {
    1_000
}

fn default_monitor_broadcast_capacity() -> usize {
    32
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

fn duration_from_env_ms(name: &str, default: Duration) -> Result<Duration> {
    match std::env::var(name) {
        Ok(value) => {
            let millis = value.parse::<u64>().map_err(|error| {
                anyhow::anyhow!("{name} must be an integer number of milliseconds: {error}")
            })?;
            Ok(Duration::from_millis(millis))
        }
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(anyhow::anyhow!("failed to read {name}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_default_settings() {
        let settings = Settings::default();
        assert!(!settings.jetstream_hosts.is_empty());
        assert_eq!(settings.wanted_collections, "app.bsky.feed.post");
        assert_eq!(settings.batch_size, 10);
        assert_eq!(settings.max_db_size_mb, 20 * 1024);
        assert_eq!(settings.max_concurrent_requests, 9);
        assert_eq!(settings.channel_capacity, 1_000);
        assert_eq!(settings.monitor_broadcast_capacity, 32);
        assert_eq!(settings.max_ingress_event_bytes, 256 * 1024);
        assert!(
            settings.conservative_memory_working_set_bytes()
                < settings.memory_envelope().recovery_bytes
        );
        assert_eq!(settings.jetstream_data_idle_timeout_secs, 30);
        assert_eq!(settings.jetstream_connect_timeout_secs, 10);
        assert_eq!(settings.jetstream_cursor_overlap_secs, 10);
        assert_eq!(settings.jetstream_endpoint_backoff_min_secs, 1);
        assert_eq!(settings.jetstream_endpoint_backoff_max_secs, 30);
        assert_eq!(settings.jetstream_committed_lag_threshold_secs, 30);
        assert_eq!(settings.jetstream_live_stability_observations, 3);
        assert!(settings.jetstream_recovery_deadlines_enabled);
        assert!(settings.jetstream_cursor_replay_enabled);
        assert_eq!(settings.vacuum_freelist_ratio, 0.10);
        assert_eq!(settings.vacuum_window_start_hour, 3);
        assert_eq!(settings.vacuum_window_end_hour, 5);
        assert_eq!(settings.vacuum_max_defer_hours, 6);
        assert_eq!(settings.batch_execution_timeout_secs, 60);
        assert_eq!(settings.pipeline_startup_grace_secs, 300);
        assert_eq!(settings.readiness_recovery_successes, 3);
        assert!(!settings.pipeline_progress_readiness_enabled);
        assert!(!settings.pipeline_deadlines_enabled);
        assert_eq!(settings.cache_size_users, 50_000);
        assert_eq!(settings.cache_size_posts, 40_000);
        assert_eq!(settings.profile_coordination_key_capacity, 150);
        assert_eq!(settings.profile_coordination_waiter_capacity, 600);
        assert_eq!(settings.post_coordination_key_capacity, 150);
        assert_eq!(settings.post_coordination_waiter_capacity, 600);
        assert_eq!(settings.negative_post_cache_capacity, 20_000);
        assert_eq!(settings.negative_post_cache_ttl, Duration::from_secs(300));
        assert_eq!(settings.sqlite_cache_size_kib, 64 * 1024);
        assert_eq!(settings.sqlite_mmap_size_mb, 256);
        assert_eq!(settings.sqlite_journal_size_limit_mb, 512);
        assert_eq!(settings.sqlite_max_connections, 4);
        assert_eq!(settings.user_cache_limit_mb, 512);
        assert_eq!(settings.post_cache_limit_mb, 1024);
        assert_eq!(settings.negative_post_cache_limit_mb, 128);
        assert_eq!(settings.memory_recovery_mb, 3072);
        assert_eq!(settings.memory_soft_pressure_mb, 3584);
        assert_eq!(settings.memory_emergency_mb, 4608);
        assert_eq!(settings.memory_external_hard_limit_mb, 5120);
        assert!(settings.memory_envelope().validate().is_ok());
        assert_eq!(settings.max_retries, 3);
        assert_eq!(settings.retry_base_delay, Duration::from_millis(100));
        assert_eq!(settings.retry_max_delay, Duration::from_secs(5));
        assert_eq!(settings.recovery_min_delay, Duration::from_secs(5));
        assert_eq!(settings.recovery_max_delay, Duration::from_secs(300));
        assert_eq!(settings.recovery_persistence_threshold, 3);
        assert_eq!(settings.isolation_request_budget, 8);
    }

    #[test]
    fn test_validation_missing_required_fields() {
        let mut settings = Settings {
            stream_name: "".to_string(),
            ..Default::default()
        };

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

    #[test]
    fn test_pipeline_settings_validation() {
        let mut settings = Settings {
            stream_name: "test".to_string(),
            bluesky_handle: "test.bsky.social".to_string(),
            bluesky_app_password: "password".to_string(),
            ..Default::default()
        };

        settings.jetstream_data_idle_timeout_secs = 0;
        assert!(settings.validate().is_err());
        settings.jetstream_data_idle_timeout_secs = 1;
        settings.jetstream_connect_timeout_secs = 0;
        assert!(settings.validate().is_err());
        settings.jetstream_connect_timeout_secs = 1;
        settings.jetstream_cursor_overlap_secs = 86_401;
        assert!(settings.validate().is_err());
        settings.jetstream_cursor_overlap_secs = 10;
        settings.jetstream_endpoint_backoff_min_secs = 31;
        settings.jetstream_endpoint_backoff_max_secs = 30;
        assert!(settings.validate().is_err());
        settings.jetstream_endpoint_backoff_min_secs = 1;
        settings.jetstream_committed_lag_threshold_secs = 0;
        assert!(settings.validate().is_err());
        settings.jetstream_committed_lag_threshold_secs = 30;
        settings.jetstream_live_stability_observations = 0;
        assert!(settings.validate().is_err());
        settings.jetstream_live_stability_observations = 3;
        settings.batch_execution_timeout_secs = 0;
        assert!(settings.validate().is_err());
        settings.batch_execution_timeout_secs = 1;
        settings.pipeline_startup_grace_secs = 0;
        assert!(settings.validate().is_err());
        settings.pipeline_startup_grace_secs = 1;
        settings.readiness_recovery_successes = 0;
        assert!(settings.validate().is_err());
        settings.readiness_recovery_successes = 1;
        assert!(settings.validate().is_ok());

        settings.retry_base_delay = Duration::ZERO;
        assert!(settings.validate().is_err());
        settings.retry_base_delay = Duration::from_millis(100);
        settings.retry_max_delay = Duration::from_millis(99);
        assert!(settings.validate().is_err());
        settings.retry_max_delay = Duration::from_millis(100);
        settings.recovery_min_delay = Duration::ZERO;
        assert!(settings.validate().is_err());
        settings.recovery_min_delay = Duration::from_secs(1);
        settings.recovery_max_delay = Duration::from_millis(999);
        assert!(settings.validate().is_err());
        settings.recovery_max_delay = Duration::from_secs(1);
        settings.recovery_persistence_threshold = 0;
        assert!(settings.validate().is_err());
        settings.recovery_persistence_threshold = 1;
        settings.isolation_request_budget = 0;
        assert!(settings.validate().is_err());
        settings.isolation_request_budget = 1;
        settings.profile_coordination_key_capacity = 0;
        assert!(settings.validate().is_err());
        settings.profile_coordination_key_capacity = settings.profile_batch_size;
        settings.post_coordination_key_capacity = settings.post_batch_size - 1;
        assert!(settings.validate().is_err());
        settings.post_coordination_key_capacity = settings.post_batch_size;
        settings.profile_coordination_waiter_capacity = settings.profile_batch_size - 1;
        assert!(settings.validate().is_err());
        settings.profile_coordination_waiter_capacity = settings.profile_batch_size;
        settings.post_coordination_waiter_capacity = settings.post_batch_size - 1;
        assert!(settings.validate().is_err());
        settings.post_coordination_waiter_capacity = settings.post_batch_size;
        settings.negative_post_cache_capacity = 0;
        assert!(settings.validate().is_err());
        settings.negative_post_cache_capacity = 1;
        settings.negative_post_cache_ttl = Duration::ZERO;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn test_pipeline_settings_load_from_environment() {
        let _guard = ENV_LOCK.lock().expect("environment test lock poisoned");
        let values = [
            ("STREAM_NAME", "test"),
            ("BLUESKY_HANDLE", "test.bsky.social"),
            ("BLUESKY_APP_PASSWORD", "password"),
            ("JETSTREAM_DATA_IDLE_TIMEOUT_SECS", "45"),
            ("JETSTREAM_CONNECT_TIMEOUT_SECS", "12"),
            ("JETSTREAM_CURSOR_OVERLAP_SECS", "15"),
            ("JETSTREAM_ENDPOINT_BACKOFF_MIN_SECS", "2"),
            ("JETSTREAM_ENDPOINT_BACKOFF_MAX_SECS", "20"),
            ("JETSTREAM_COMMITTED_LAG_THRESHOLD_SECS", "25"),
            ("JETSTREAM_LIVE_STABILITY_OBSERVATIONS", "4"),
            ("JETSTREAM_RECOVERY_DEADLINES_ENABLED", "false"),
            ("JETSTREAM_CURSOR_REPLAY_ENABLED", "false"),
            ("BATCH_EXECUTION_TIMEOUT_SECS", "30"),
            ("PIPELINE_STARTUP_GRACE_SECS", "90"),
            ("READINESS_RECOVERY_SUCCESSES", "5"),
            ("PIPELINE_PROGRESS_READINESS_ENABLED", "true"),
            ("PIPELINE_DEADLINES_ENABLED", "false"),
            ("VACUUM_FREELIST_RATIO", "0.25"),
            ("VACUUM_WINDOW_START_HOUR", "1"),
            ("VACUUM_WINDOW_END_HOUR", "2"),
            ("VACUUM_MAX_DEFER_HOURS", "12"),
            ("BLUESKY_MAX_RETRIES", "0"),
            ("BLUESKY_RETRY_BASE_DELAY_MS", "25"),
            ("BLUESKY_RETRY_MAX_DELAY_MS", "250"),
            ("BLUESKY_RECOVERY_MIN_DELAY_MS", "1000"),
            ("BLUESKY_RECOVERY_MAX_DELAY_MS", "9000"),
            ("BLUESKY_RECOVERY_PERSISTENCE_THRESHOLD", "4"),
            ("BLUESKY_ISOLATION_REQUEST_BUDGET", "6"),
            ("BLUESKY_NEGATIVE_POST_CACHE_CAPACITY", "1234"),
            ("BLUESKY_NEGATIVE_POST_CACHE_TTL_MS", "45000"),
            ("BLUESKY_PROFILE_COORDINATION_KEY_CAPACITY", "101"),
            ("BLUESKY_PROFILE_COORDINATION_WAITER_CAPACITY", "401"),
            ("BLUESKY_POST_COORDINATION_KEY_CAPACITY", "102"),
            ("BLUESKY_POST_COORDINATION_WAITER_CAPACITY", "402"),
            ("SQLITE_MAX_CONNECTIONS", "3"),
            ("USER_CACHE_LIMIT_MB", "111"),
            ("POST_CACHE_LIMIT_MB", "222"),
            ("NEGATIVE_POST_CACHE_LIMIT_MB", "33"),
            ("MEMORY_RECOVERY_MB", "2500"),
            ("MEMORY_SOFT_PRESSURE_MB", "2800"),
            ("MEMORY_EMERGENCY_MB", "3000"),
            ("MEMORY_EXTERNAL_HARD_LIMIT_MB", "4000"),
            ("MEMORY_HOST_TOTAL_MB", "6000"),
            ("MEMORY_REQUIRED_HOST_HEADROOM_MB", "2000"),
            ("MEMORY_PRESSURE_CONFIRMATION_SECS", "7"),
            ("MEMORY_RECOVERY_CONFIRMATION_SECS", "9"),
            ("MEMORY_SAMPLE_INTERVAL_SECS", "2"),
            ("MEMORY_SAMPLE_CAPACITY", "44"),
            ("IN_FLIGHT_PAYLOAD_LIMIT_MB", "300"),
            ("MAX_INGRESS_EVENT_BYTES", "1048576"),
            ("MEMORY_PRESSURE_ACTIONS_ENABLED", "true"),
            ("MEMORY_EMERGENCY_EXIT_ENABLED", "true"),
        ];
        for (key, value) in values {
            std::env::set_var(key, value);
        }

        let settings = Settings::from_env().expect("pipeline settings should load");

        for (key, _) in values {
            std::env::remove_var(key);
        }
        assert_eq!(settings.jetstream_data_idle_timeout_secs, 45);
        assert_eq!(settings.jetstream_connect_timeout_secs, 12);
        assert_eq!(settings.jetstream_cursor_overlap_secs, 15);
        assert_eq!(settings.jetstream_endpoint_backoff_min_secs, 2);
        assert_eq!(settings.jetstream_endpoint_backoff_max_secs, 20);
        assert_eq!(settings.jetstream_committed_lag_threshold_secs, 25);
        assert_eq!(settings.jetstream_live_stability_observations, 4);
        assert!(!settings.jetstream_recovery_deadlines_enabled);
        assert!(!settings.jetstream_cursor_replay_enabled);
        assert_eq!(settings.batch_execution_timeout_secs, 30);
        assert_eq!(settings.pipeline_startup_grace_secs, 90);
        assert_eq!(settings.readiness_recovery_successes, 5);
        assert!(settings.pipeline_progress_readiness_enabled);
        assert!(!settings.pipeline_deadlines_enabled);
        assert_eq!(settings.vacuum_freelist_ratio, 0.25);
        assert_eq!(settings.vacuum_window_start_hour, 1);
        assert_eq!(settings.vacuum_window_end_hour, 2);
        assert_eq!(settings.vacuum_max_defer_hours, 12);
        assert_eq!(settings.max_retries, 0);
        assert_eq!(settings.retry_base_delay, Duration::from_millis(25));
        assert_eq!(settings.retry_max_delay, Duration::from_millis(250));
        assert_eq!(settings.recovery_min_delay, Duration::from_secs(1));
        assert_eq!(settings.recovery_max_delay, Duration::from_secs(9));
        assert_eq!(settings.recovery_persistence_threshold, 4);
        assert_eq!(settings.isolation_request_budget, 6);
        assert_eq!(settings.negative_post_cache_capacity, 1234);
        assert_eq!(settings.negative_post_cache_ttl, Duration::from_secs(45));
        assert_eq!(settings.profile_coordination_key_capacity, 101);
        assert_eq!(settings.profile_coordination_waiter_capacity, 401);
        assert_eq!(settings.post_coordination_key_capacity, 102);
        assert_eq!(settings.post_coordination_waiter_capacity, 402);
        assert_eq!(settings.sqlite_max_connections, 3);
        assert_eq!(settings.user_cache_limit_mb, 111);
        assert_eq!(settings.post_cache_limit_mb, 222);
        assert_eq!(settings.negative_post_cache_limit_mb, 33);
        assert_eq!(settings.memory_recovery_mb, 2500);
        assert_eq!(settings.memory_soft_pressure_mb, 2800);
        assert_eq!(settings.memory_emergency_mb, 3000);
        assert_eq!(settings.memory_external_hard_limit_mb, 4000);
        assert_eq!(settings.memory_sample_interval_secs, 2);
        assert_eq!(settings.memory_sample_capacity, 44);
        assert!(settings.memory_pressure_actions_enabled);
        assert!(settings.memory_emergency_exit_enabled);
    }

    #[test]
    fn hydration_execution_mode_env_override_is_validated() {
        let _guard = ENV_LOCK.lock().expect("environment test lock poisoned");
        std::env::set_var("STREAM_NAME", "test");
        std::env::set_var("BLUESKY_HANDLE", "test.bsky.social");
        std::env::set_var("BLUESKY_APP_PASSWORD", "password");

        std::env::set_var("HYDRATION_EXECUTION_MODE", "parallel");
        let settings = Settings::from_env().expect("settings should load with parallel mode");
        assert_eq!(
            settings.hydration_execution_mode,
            HydrationExecutionMode::Parallel
        );

        std::env::set_var("HYDRATION_EXECUTION_MODE", "sequential");
        let settings = Settings::from_env().expect("settings should load with sequential mode");
        assert_eq!(
            settings.hydration_execution_mode,
            HydrationExecutionMode::Sequential
        );
        std::env::remove_var("HYDRATION_EXECUTION_MODE");

        let default = Settings::default();
        assert_eq!(
            default.hydration_execution_mode,
            HydrationExecutionMode::Parallel,
            "concurrent resolution is the deployment default after the canary promotion; \
             rollback is HYDRATION_EXECUTION_MODE=sequential"
        );

        std::env::set_var("HYDRATION_EXECUTION_MODE", "unbounded-chaos");
        let error = Settings::from_env().expect_err("invalid mode must be rejected");
        std::env::remove_var("HYDRATION_EXECUTION_MODE");
        for key in ["STREAM_NAME", "BLUESKY_HANDLE", "BLUESKY_APP_PASSWORD"] {
            std::env::remove_var(key);
        }
        assert!(
            error.to_string().to_lowercase().contains("hydration")
                || error.to_string().to_lowercase().contains("unknown_variant")
                || error.to_string().to_lowercase().contains("deserialize"),
            "unexpected error for invalid hydration mode: {error}"
        );
    }

    #[test]
    fn runtime_memory_settings_reject_invalid_envelopes_and_bounds() {
        let valid = Settings {
            stream_name: "test".to_string(),
            bluesky_handle: "test.bsky.social".to_string(),
            bluesky_app_password: "password".to_string(),
            ..Default::default()
        };
        assert!(valid.validate().is_ok());

        let mut invalid_order = valid.clone();
        invalid_order.memory_soft_pressure_mb = invalid_order.memory_recovery_mb;
        assert!(invalid_order.validate().is_err());

        let mut invalid_headroom = valid.clone();
        invalid_headroom.memory_required_host_headroom_mb = 4096;
        assert!(invalid_headroom.validate().is_err());

        let mut invalid_pool = valid.clone();
        invalid_pool.sqlite_max_connections = 0;
        assert!(invalid_pool.validate().is_err());

        let mut invalid_payload = valid;
        invalid_payload.in_flight_payload_limit_mb = 1;
        assert!(invalid_payload.validate().is_err());
    }

    #[test]
    fn permit_default_of_nine_keeps_payload_coverage_bounded() {
        let settings = Settings {
            stream_name: "test".to_string(),
            bluesky_handle: "test.bsky.social".to_string(),
            bluesky_app_password: "password".to_string(),
            ..Default::default()
        };
        // The default in-flight payload limit (256 MB) must still cover the
        // raised permit count: 9 permits * 25 records * 256 KiB ≈ 57.6 MB.
        assert!(settings.validate().is_ok());

        let mut undersized = settings.clone();
        undersized.in_flight_payload_limit_mb = 50;
        assert!(
            undersized.validate().is_err(),
            "50 MB cannot cover 9 permits * 25 records * max_ingress_event_bytes"
        );
    }
}
