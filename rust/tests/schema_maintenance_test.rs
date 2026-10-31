use jetstream_turbo_rs::models::TurboError;
use jetstream_turbo_rs::storage::{
    verify_required_indexes, SQLitePragmaConfig, SQLiteStore, SchemaMaintenanceError,
    SchemaVerification, REQUIRED_INDEXES,
};
use jetstream_turbo_rs::turbocharger::{CgroupMemoryDiagnostics, MemoryEnvelope};
use jetstream_turbo_rs::utils::logging::retention::{LogOwner, LogPolicy};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LARGE_FIXTURE_ROWS: usize = 100_000;
const VERIFICATION_BUDGET: Duration = Duration::from_secs(1);
const STARTUP_BUDGET: Duration = Duration::from_secs(2);
const MIB: u64 = 1024 * 1024;

fn startup_memory_envelope(
    host_memory_bytes: Option<u64>,
    cgroup: &CgroupMemoryDiagnostics,
) -> MemoryEnvelope {
    let defaults = jetstream_turbo_rs::Settings::default().memory_envelope();
    // This fixture tests schema startup, not memory acceptance. Its child still
    // observes the real cgroup, which can include unrelated CI jobs and cache.
    // Cover the entire reported capacity, not just an instantaneous usage sample.
    let capacity_bytes = defaults
        .host_memory_bytes
        .max(host_memory_bytes.unwrap_or(0))
        .max(cgroup.max_bytes.unwrap_or(0))
        .max(cgroup.current_bytes.unwrap_or(0));
    let recovery_mb = capacity_bytes.div_ceil(MIB) + 1024;
    MemoryEnvelope {
        recovery_bytes: recovery_mb * MIB,
        soft_pressure_bytes: (recovery_mb + 1024) * MIB,
        emergency_bytes: (recovery_mb + 2048) * MIB,
        external_hard_limit_bytes: (recovery_mb + 3072) * MIB,
        host_memory_bytes: (recovery_mb + 3072) * MIB + defaults.required_host_headroom_bytes,
        ..defaults
    }
}

#[test]
fn startup_envelope_covers_shared_runner_capacity_with_valid_headroom() {
    for (host_mb, limit_mb, usage_mb) in [
        (Some(16_384), Some(8192), Some(6144)),
        (Some(16_384), None, Some(6144)),
        (Some(8192), Some(32_768), Some(10_240)),
        (None, None, Some(12_288)),
        (None, None, None),
    ] {
        let host = host_mb.map(|mb| mb * MIB);
        let cgroup = CgroupMemoryDiagnostics {
            max_bytes: limit_mb.map(|mb| mb * MIB),
            current_bytes: usage_mb.map(|mb| mb * MIB),
            ..CgroupMemoryDiagnostics::default()
        };
        let envelope = startup_memory_envelope(host, &cgroup);
        envelope.validate().expect("valid startup fixture envelope");
        let capacity = [host, cgroup.max_bytes, cgroup.current_bytes]
            .into_iter()
            .flatten()
            .max()
            .unwrap_or(0);
        assert!(envelope.recovery_bytes > capacity);
        assert!(
            envelope.recovery_bytes
                > jetstream_turbo_rs::Settings::default().conservative_memory_working_set_bytes()
        );
    }
}

async fn run_schema_maintenance(directory: &std::path::Path) -> std::process::Output {
    let output = Command::new(env!("CARGO_BIN_EXE_jetstream-turbo"))
        .current_dir(directory)
        .env("TURBO__DB_DIR", directory)
        .env("RUST_LOG", "info")
        .arg("schema-maintenance")
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "maintenance failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Starting offline schema maintenance"));
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("Application file logging unavailable"),
        "maintenance must select stdout directly rather than fail file initialization"
    );
    output
}

#[tokio::test]
async fn schema_maintenance_uses_stdout_without_creating_a_log_directory() {
    let directory = tempfile::tempdir().unwrap();
    run_schema_maintenance(directory.path()).await;
    assert!(!directory.path().join("logs").exists());
}

#[tokio::test]
async fn schema_maintenance_does_not_touch_an_existing_file_log_owner() {
    let directory = tempfile::tempdir().unwrap();
    let logs = directory.path().join("logs");
    std::fs::create_dir(&logs).unwrap();
    let segment = logs.join(format!(
        "jetstream-turbo.main.{}.{}.log",
        chrono::Utc::now().timestamp(),
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&segment, b"service log").unwrap();
    let owner = LogOwner::start(&logs, LogPolicy::default()).unwrap();
    let snapshot = || {
        std::fs::read_dir(&logs)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), std::fs::read(entry.path()).unwrap())
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let before = snapshot();
    run_schema_maintenance(directory.path()).await;
    assert_eq!(snapshot(), before);
    assert_eq!(owner.diagnostics().allocated_bytes, 11);
}

fn test_pragma_config() -> SQLitePragmaConfig {
    SQLitePragmaConfig {
        cache_size_kib: 16 * 1024,
        mmap_size_mb: 64,
        journal_size_limit_mb: 64,
    }
}

async fn create_records_table(pool: &SqlitePool) {
    sqlx::query(
        r#"
        CREATE TABLE records (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            at_uri TEXT,
            did TEXT,
            time_us INTEGER,
            source_event_id TEXT,
            message TEXT NOT NULL,
            message_metadata TEXT,
            created_at TEXT NOT NULL,
            hydrated_at TEXT NOT NULL,
            hydration_time_ms INTEGER,
            api_calls_count INTEGER,
            cache_hit_rate REAL,
            cache_hits INTEGER,
            cache_misses INTEGER,
            hydration_quality TEXT NOT NULL DEFAULT 'unknown'
        )
        "#,
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn create_large_unmaintained_fixture(path: &std::path::Path) -> SqlitePool {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    create_records_table(&pool).await;
    sqlx::query(
        r#"
        WITH digits(value) AS (
            VALUES (0), (1), (2), (3), (4), (5), (6), (7), (8), (9)
        )
        INSERT INTO records (
            at_uri, did, time_us, source_event_id, message, message_metadata,
            created_at, hydrated_at, hydration_quality
        )
        SELECT
            'at://did:plc:test/app.bsky.feed.post/' || row_number,
            'did:plc:' || (row_number % 1000),
            row_number,
            'event-' || row_number,
            '{}',
            '{}',
            '2026-01-01T00:00:00Z',
            '2026-01-01T00:00:00Z',
            'complete'
        FROM (
            SELECT
                a.value + b.value * 10 + c.value * 100 + d.value * 1000
                    + e.value * 10000 + 1 AS row_number
            FROM digits a
            CROSS JOIN digits b
            CROSS JOIN digits c
            CROSS JOIN digits d
            CROSS JOIN digits e
        )
        LIMIT 100000
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM records")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count as usize, LARGE_FIXTURE_ROWS);
    pool
}

#[tokio::test]
async fn large_unmaintained_database_fails_promptly_without_creating_indexes() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("large-unmaintained.db");
    let pool = create_large_unmaintained_fixture(&db_path).await;

    let verification_started = Instant::now();
    let verification = verify_required_indexes(&pool).await.unwrap();
    let verification_elapsed = verification_started.elapsed();
    pool.close().await;

    let startup_started = Instant::now();
    let error = match SQLiteStore::new(&db_path, test_pragma_config()).await {
        Ok(store) => {
            store.close().await.unwrap();
            panic!("unmaintained schema unexpectedly opened")
        }
        Err(error) => error,
    };
    let startup_elapsed = startup_started.elapsed();

    let options = SqliteConnectOptions::new().filename(&db_path);
    let inspection_pool = SqlitePool::connect_with(options).await.unwrap();
    let created_indexes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'index' AND name LIKE 'idx_records_%'",
    )
    .fetch_one(&inspection_pool)
    .await
    .unwrap();

    assert!(matches!(
        verification,
        SchemaVerification::MaintenanceRequired { .. }
    ));
    assert!(verification_elapsed < VERIFICATION_BUDGET);
    assert!(matches!(
        error,
        TurboError::SchemaMaintenanceRequired { .. }
    ));
    assert!(startup_elapsed < STARTUP_BUDGET);
    assert_eq!(created_indexes, 0);
}

#[tokio::test]
async fn maintained_large_database_opens_store_within_startup_budget() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("large-maintained.db");
    let pool = create_large_unmaintained_fixture(&db_path).await;
    pool.close().await;

    let maintenance_started = Instant::now();
    SQLiteStore::maintain_schema(&db_path, test_pragma_config(), Duration::from_secs(5))
        .await
        .unwrap();
    let maintenance_elapsed = maintenance_started.elapsed();

    let startup_started = Instant::now();
    let store = SQLiteStore::new(&db_path, test_pragma_config())
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let startup_elapsed = startup_started.elapsed();

    eprintln!(
        "large fixture timings: maintenance={maintenance_elapsed:?}, verified_startup_and_bind={startup_elapsed:?}"
    );
    assert!(startup_elapsed < STARTUP_BUDGET);
    drop(listener);
    store.close().await.unwrap();
}

#[tokio::test]
async fn production_binary_passes_readiness_before_scheduled_cleanup() {
    assert_production_startup(false).await;
}

#[tokio::test]
async fn disk_pressure_startup_serves_diagnostics_without_admitting_growth() {
    assert_production_startup(true).await;
}

async fn assert_production_startup(disk_pressure: bool) {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("jetstream.db");
    let pool = create_large_unmaintained_fixture(&db_path).await;
    pool.close().await;
    SQLiteStore::maintain_schema(&db_path, test_pragma_config(), Duration::from_secs(5))
        .await
        .unwrap();

    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/com.atproto.server.createSession"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "accessJwt": "test-access-token",
            "refreshJwt": "test-refresh-token",
            "handle": "test.bsky.social",
            "did": "did:plc:test"
        })))
        .mount(&auth_server)
        .await;

    let port_probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_port = port_probe.local_addr().unwrap().port();
    drop(port_probe);

    let host_memory_bytes = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|contents| {
            contents.lines().find_map(|line| {
                line.strip_prefix("MemTotal:")?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()?
                    .checked_mul(1024)
            })
        });
    let envelope = startup_memory_envelope(host_memory_bytes, &CgroupMemoryDiagnostics::collect());
    let mut child = Command::new(env!("CARGO_BIN_EXE_jetstream-turbo"))
        .current_dir(temp_dir.path())
        .env("BLUESKY_HANDLE", "test.bsky.social")
        .env("BLUESKY_APP_PASSWORD", "test-password")
        .env("STREAM_NAME", "test-stream")
        .env("JETSTREAM_HOSTS", r#"["ws://127.0.0.1:9"]"#)
        .env("MAX_DB_SIZE_MB", "6144")
        // Exercise startup on CI disks without requiring the production 16 GiB
        // reserve. The storage policy still adds its 2 GiB growth allowance.
        .env(
            "DISK_MIN_FREE_MB",
            if disk_pressure { "1000000000" } else { "1" },
        )
        .env("CLEANUP_CHUNK_SIZE", "1")
        .env("CLEANUP_CHUNK_DELAY_MS", "1000")
        .env("CLEANUP_CHECK_INTERVAL_MINUTES", "60")
        .envs(
            [
                ("MEMORY_RECOVERY_MB", envelope.recovery_bytes),
                ("MEMORY_SOFT_PRESSURE_MB", envelope.soft_pressure_bytes),
                ("MEMORY_EMERGENCY_MB", envelope.emergency_bytes),
                (
                    "MEMORY_EXTERNAL_HARD_LIMIT_MB",
                    envelope.external_hard_limit_bytes,
                ),
                ("MEMORY_HOST_TOTAL_MB", envelope.host_memory_bytes),
                (
                    "MEMORY_REQUIRED_HOST_HEADROOM_MB",
                    envelope.required_host_headroom_bytes,
                ),
            ]
            .map(|(name, bytes)| (name, (bytes / MIB).to_string())),
        )
        .env("TURBO__BLUESKY_API_URL", auth_server.uri())
        .env("TURBO__DB_DIR", temp_dir.path())
        .env("TURBO__HTTP_PORT", http_port.to_string())
        .env("RUST_LOG", "warn")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(100))
        .build()
        .unwrap();
    let deadline = Instant::now() + STARTUP_BUDGET;
    let mut listener_available = false;
    let mut last_readiness = String::from("no readiness response");
    while Instant::now() < deadline {
        match client
            .get(format!("http://127.0.0.1:{http_port}/ready"))
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();
                last_readiness = format!("HTTP {status}");
                let expected = if disk_pressure {
                    reqwest::StatusCode::SERVICE_UNAVAILABLE
                } else {
                    reqwest::StatusCode::OK
                };
                if status == expected {
                    listener_available = true;
                    break;
                }
            }
            Err(error) => last_readiness = error.to_string(),
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    if disk_pressure && listener_available {
        let health: serde_json::Value = client
            .get(format!("http://127.0.0.1:{http_port}/api/v1/health"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health["data"]["readiness"]["reason"], "disk_pressure");
        assert_eq!(
            health["data"]["diagnostics"]["sqlite_state"]["capacity"]["admission_allowed"],
            false
        );
    }
    let failure_diagnostics = if listener_available {
        String::new()
    } else {
        match client
            .get(format!("http://127.0.0.1:{http_port}/api/v1/health"))
            .send()
            .await
        {
            Ok(response) => match response.json::<serde_json::Value>().await {
                Ok(health) => serde_json::json!({
                    "readiness": health["data"]["readiness"],
                    "capacity": health["data"]["diagnostics"]["sqlite_state"]["capacity"],
                    "memory_pressure": health["data"]["diagnostics"]["runtime_memory"]["pressure_state"],
                }).to_string(),
                Err(error) => format!("health response decode failed: {error}"),
            },
            Err(error) => format!("health request failed: {error}"),
        }
    };
    let killed_by_test = child.try_wait().unwrap().is_none();
    if killed_by_test {
        let _ = child.start_kill();
    }
    let output = child.wait_with_output().await.unwrap();
    assert!(
        listener_available,
        "production binary did not become ready within {STARTUP_BUDGET:?}; \
         last_readiness={last_readiness}; health={failure_diagnostics}; \
         killed_by_test={killed_by_test}; status={:?}; stdout={}; stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn partial_maintenance_state_is_safe_to_retry() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("interrupted.db");
    let options = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    create_records_table(&pool).await;
    sqlx::query(REQUIRED_INDEXES[0].sql)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let report =
        SQLiteStore::maintain_schema(&db_path, test_pragma_config(), Duration::from_secs(1))
            .await
            .unwrap();

    assert_eq!(report.created_indexes.len(), REQUIRED_INDEXES.len() - 1);
}

#[tokio::test]
async fn maintenance_does_not_rewrite_existing_records() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("legacy-hydration-quality.db");
    let options = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    create_records_table(&pool).await;
    sqlx::query(
        r#"
        INSERT INTO records (
            message, created_at, hydrated_at, hydration_quality
        ) VALUES ('{}', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 'legacy-value')
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    SQLiteStore::maintain_schema(&db_path, test_pragma_config(), Duration::from_secs(1))
        .await
        .unwrap();

    let options = SqliteConnectOptions::new().filename(&db_path);
    let inspection_pool = SqlitePool::connect_with(options).await.unwrap();
    let stored_quality: String = sqlx::query_scalar("SELECT hydration_quality FROM records")
        .fetch_one(&inspection_pool)
        .await
        .unwrap();
    assert_eq!(stored_quality, "legacy-value");
}

#[tokio::test]
async fn maintenance_returns_typed_lock_timeout() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("locked.db");
    SQLiteStore::maintain_schema(&db_path, test_pragma_config(), Duration::from_secs(1))
        .await
        .unwrap();

    let options = SqliteConnectOptions::new().filename(&db_path);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::query("DROP INDEX idx_records_did")
        .execute(&pool)
        .await
        .unwrap();
    let mut lock = pool.acquire().await.unwrap();
    sqlx::query("BEGIN EXCLUSIVE")
        .execute(&mut *lock)
        .await
        .unwrap();

    let error =
        SQLiteStore::maintain_schema(&db_path, test_pragma_config(), Duration::from_millis(50))
            .await
            .unwrap_err();

    sqlx::query("ROLLBACK").execute(&mut *lock).await.unwrap();
    assert!(matches!(error, SchemaMaintenanceError::LockTimeout { .. }));
}
