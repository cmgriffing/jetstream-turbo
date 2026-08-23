use jetstream_turbo_rs::models::TurboError;
use jetstream_turbo_rs::storage::{
    verify_required_indexes, SQLitePragmaConfig, SQLiteStore, SchemaMaintenanceError,
    SchemaVerification, REQUIRED_INDEXES,
};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::time::{Duration, Instant};

const LARGE_FIXTURE_ROWS: usize = 100_000;
const VERIFICATION_BUDGET: Duration = Duration::from_secs(1);
const STARTUP_BUDGET: Duration = Duration::from_secs(2);

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
async fn maintained_large_database_reaches_listener_step_within_startup_budget() {
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
