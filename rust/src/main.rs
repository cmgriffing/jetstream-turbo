use anyhow::Result;
use clap::{Parser, Subcommand};
use jetstream_turbo_rs::config::Settings;
use jetstream_turbo_rs::server::create_server;
use jetstream_turbo_rs::storage::{SQLitePragmaConfig, SQLiteStore};
use jetstream_turbo_rs::telemetry::ErrorReporter;
use jetstream_turbo_rs::turbocharger::ProductionTurboCharger as TurboCharger;
use std::any::Any;
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::time::Duration;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::{filter::filter_fn, layer::SubscriberExt, util::SubscriberInitExt, Layer};

const BATCH_REPORT_LOG_TARGET: &str = "jetstream_turbo.batch_report";

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Turbocharged Jetstream messages - hydrates referenced objects and stores to SQLite",
    long_about = r#"
Turbocharged Jetstream messages - hydrates referenced objects and stores to SQLite.

SETUP:
    1. Copy .env.example to .env
    2. Set BLUESKY_HANDLE and BLUESKY_APP_PASSWORD in .env
       (Get an app password at: https://bsky.app/settings/app-passwords)
    3. Run: cargo run

EXAMPLES:
    cargo run
    cargo run -- --log-level debug
    cargo run -- --modulo 4 --shard 0

For more information, see README.md
"#
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Shard modulo for distributed processing (0 = single instance)
    #[arg(short, long, default_value_t = 0)]
    modulo: u32,

    /// Shard index (0 to modulo-1) for this instance
    #[arg(short, long, default_value_t = 0)]
    shard: u32,

    /// Log level: trace, debug, info, warn, error
    #[arg(long)]
    log_level: Option<String>,
}

#[derive(Subcommand, Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    /// Reconcile required SQLite indexes without starting the service.
    SchemaMaintenance {
        /// Override the configured SQLite lock wait bound.
        #[arg(long)]
        busy_timeout_secs: Option<u64>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Install rustls crypto provider
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|e| anyhow::anyhow!("Failed to install rustls crypto provider: {e:?}"))?;

    let Args {
        command,
        modulo,
        shard,
        log_level,
    } = Args::parse();

    // Default to warn in release mode, info in debug mode
    let log_level = log_level.unwrap_or_else(|| {
        if cfg!(debug_assertions) {
            "info".to_string()
        } else {
            "warn".to_string()
        }
    });

    // Initialize tracing
    let _log_guards = init_tracing(&log_level)?;

    if let Some(Command::SchemaMaintenance { busy_timeout_secs }) = command {
        let settings = Settings::from_env_for_schema_maintenance()?;
        let busy_timeout = Duration::from_secs(
            busy_timeout_secs.unwrap_or(settings.sqlite_schema_maintenance_busy_timeout_secs),
        );
        tracing::info!(
            database_path = %settings.database_path().display(),
            busy_timeout_secs = busy_timeout.as_secs(),
            "Starting offline schema maintenance"
        );
        SQLiteStore::maintain_schema(
            settings.database_path(),
            SQLitePragmaConfig {
                cache_size_kib: settings.sqlite_cache_size_kib,
                mmap_size_mb: settings.sqlite_mmap_size_mb,
                journal_size_limit_mb: settings.sqlite_journal_size_limit_mb,
            },
            busy_timeout,
        )
        .await?;
        return Ok(());
    }

    let settings = Settings::from_env()?;

    SQLiteStore::verify_schema_ready(
        settings.database_path(),
        SQLitePragmaConfig {
            cache_size_kib: settings.sqlite_cache_size_kib,
            mmap_size_mb: settings.sqlite_mmap_size_mb,
            journal_size_limit_mb: settings.sqlite_journal_size_limit_mb,
        },
    )
    .await?;

    // Initialize error reporter
    let error_reporter = ErrorReporter::new(
        settings.posthog_api_key.clone(),
        settings.posthog_host.clone(),
    )
    .await;
    install_panic_hook(error_reporter.clone());

    tracing::info!("Starting jetstream-turbo v{}", env!("CARGO_PKG_VERSION"));
    tracing::info!("Configuration loaded: modulo={}, shard={}", modulo, shard);

    // Create turbocharger
    let turbocharger =
        TurboCharger::new(settings.clone(), modulo, shard, error_reporter.clone()).await?;
    let turbocharger = std::sync::Arc::new(turbocharger);

    // Start background session refresh task
    turbocharger.start_session_refresh_task();

    // Start background database cleanup task
    turbocharger.start_db_cleanup_task();

    // Run initial cleanup check on startup
    if let Err(e) = turbocharger.check_and_cleanup_db().await {
        tracing::warn!("Initial database cleanup check failed: {}", e);
    }

    // Run both turbocharger and server
    let turbocharger_clone = turbocharger.clone();
    let error_reporter_clone = error_reporter.clone();
    let turbocharger_handle = tokio::spawn(async move {
        loop {
            let restart_delay = match turbocharger_clone.run().await {
                Ok(()) => {
                    tracing::warn!("Turbocharger run loop ended unexpectedly; restarting");
                    turbocharger_clone.minimum_recovery_delay()
                }
                Err(failure) => {
                    let decision = turbocharger_clone.record_run_failure(&failure).await;
                    if decision.log_terminal {
                        let mut ctx = HashMap::new();
                        ctx.insert("component", "main");
                        ctx.insert("operation", "turbocharger_run");
                        error_reporter_clone.capture_error(failure.error(), ctx);
                    }
                    decision.delay
                }
            };

            tracing::warn!(
                recovery_delay_ms = restart_delay.as_millis(),
                "Restarting turbocharger run loop after containment delay"
            );
            tokio::time::sleep(restart_delay).await;
        }
    });

    let server_error_reporter = error_reporter.clone();
    let server_handle = tokio::spawn(async move {
        if let Err(e) = create_server(settings.http_port, turbocharger).await {
            tracing::error!("Server failed: {}", e);
            let mut ctx = HashMap::new();
            ctx.insert("component", "main");
            ctx.insert("operation", "server_run");
            server_error_reporter.capture_error(
                &jetstream_turbo_rs::TurboError::Internal(e.to_string()),
                ctx,
            );
        }
    });

    // Wait for either task to complete, then make a bounded attempt to flush telemetry.
    let shutdown_reason = tokio::select! {
        result = turbocharger_handle => {
            handle_task_exit("turbocharger", result, &error_reporter)
        }
        result = server_handle => {
            handle_task_exit("server", result, &error_reporter)
        }
    };

    if error_reporter
        .flush_with_timeout(Duration::from_secs(2))
        .await
    {
        tracing::info!(
            "Telemetry flush completed before shutdown (triggered by {} task exit)",
            shutdown_reason
        );
    } else {
        tracing::warn!(
            "Telemetry flush did not complete before shutdown (triggered by {} task exit)",
            shutdown_reason
        );
    }

    Ok(())
}

fn install_panic_hook(error_reporter: ErrorReporter) {
    let default_hook = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |panic_info| {
        let panic_message = panic_payload_to_string(panic_info.payload());
        let panic_location = panic_info
            .location()
            .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()));

        let mut context = HashMap::new();
        context.insert("component", "runtime");
        context.insert("operation", "panic_hook");
        if let Some(location) = panic_location.as_deref() {
            context.insert("panic_location", location);
        }

        error_reporter.capture_unhandled_failure("Panic", &panic_message, context);

        // Best-effort flush request; this is non-blocking and bounded in the async path.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let reporter = error_reporter.clone();
            handle.spawn(async move {
                let _ = reporter.flush_with_timeout(Duration::from_secs(2)).await;
            });
        }

        default_hook(panic_info);
    }));
}

fn handle_task_exit(
    task_name: &'static str,
    result: Result<(), tokio::task::JoinError>,
    error_reporter: &ErrorReporter,
) -> &'static str {
    match result {
        Ok(()) => {
            tracing::warn!("{} task exited", task_name);
        }
        Err(join_err) => {
            let is_panic = join_err.is_panic();
            let message = if is_panic {
                let panic_payload = join_err.into_panic();
                panic_payload_to_string(panic_payload.as_ref())
            } else {
                join_err.to_string()
            };

            let mut context = HashMap::new();
            context.insert("component", "main");
            context.insert("operation", "task_join");
            context.insert("task", task_name);
            context.insert("is_panic", if is_panic { "true" } else { "false" });

            error_reporter.capture_unhandled_failure(
                if is_panic {
                    "TaskPanic"
                } else {
                    "TaskJoinFailure"
                },
                &message,
                context,
            );
            tracing::error!("{} task failed: {}", task_name, message);
        }
    }

    task_name
}

fn panic_payload_to_string(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else {
        "unknown panic payload".to_string()
    }
}

fn init_tracing(log_level: &str) -> Result<Vec<WorkerGuard>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level));

    let stdout_layer = tracing_subscriber::fmt::layer().json();
    let main_file_filter = filter_fn(|metadata| metadata.target() != BATCH_REPORT_LOG_TARGET);
    let batch_file_filter = filter_fn(|metadata| metadata.target() == BATCH_REPORT_LOG_TARGET);

    let main_file_logging = create_file_log_writer(None);
    let batch_file_logging = create_file_log_writer(Some("batches"));

    match (main_file_logging, batch_file_logging) {
        (
            Some((main_file_writer, main_guard, main_log_path)),
            Some((batch_file_writer, batch_guard, batch_log_path)),
        ) => {
            let file_layer = tracing_subscriber::fmt::layer()
                .json()
                .with_writer(main_file_writer)
                .with_filter(main_file_filter);
            let batch_file_layer = tracing_subscriber::fmt::layer()
                .json()
                .with_writer(batch_file_writer)
                .with_filter(batch_file_filter);

            tracing_subscriber::registry()
                .with(filter)
                .with(stdout_layer)
                .with(file_layer)
                .with(batch_file_layer)
                .init();

            tracing::info!(log_path = %main_log_path.display(), "File logging enabled");
            tracing::info!(batch_log_path = %batch_log_path.display(), "Batch file logging enabled");
            Ok(vec![main_guard, batch_guard])
        }
        (Some((file_writer, guard, log_path)), None) => {
            let file_layer = tracing_subscriber::fmt::layer()
                .json()
                .with_writer(file_writer)
                .with_filter(main_file_filter);

            tracing_subscriber::registry()
                .with(filter)
                .with(stdout_layer)
                .with(file_layer)
                .init();

            tracing::info!(log_path = %log_path.display(), "File logging enabled");
            Ok(vec![guard])
        }
        (None, Some((batch_file_writer, batch_guard, batch_log_path))) => {
            let batch_file_layer = tracing_subscriber::fmt::layer()
                .json()
                .with_writer(batch_file_writer)
                .with_filter(batch_file_filter);

            tracing_subscriber::registry()
                .with(filter)
                .with(stdout_layer)
                .with(batch_file_layer)
                .init();

            tracing::info!(batch_log_path = %batch_log_path.display(), "Batch file logging enabled");
            Ok(vec![batch_guard])
        }
        (None, None) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stdout_layer)
                .init();

            Ok(Vec::new())
        }
    }
}

fn create_file_log_writer(suffix: Option<&str>) -> Option<(NonBlocking, WorkerGuard, PathBuf)> {
    let log_path = default_log_path(suffix)?;
    create_file_log_writer_at(&log_path)
}

fn create_file_log_writer_at(
    log_path: &std::path::Path,
) -> Option<(NonBlocking, WorkerGuard, PathBuf)> {
    let parent = log_path.parent()?;
    let file_name = log_path.file_name()?.to_str()?;

    if std::fs::create_dir_all(parent).is_err() {
        return None;
    }

    let appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::NEVER)
        .filename_prefix(file_name)
        .build(parent)
        .ok()?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    Some((writer, guard, log_path.to_path_buf()))
}

fn default_log_path(suffix: Option<&str>) -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let executable_name = executable.file_stem()?.to_str()?;
    let file_name = match suffix {
        Some(suffix) => format!("{executable_name}-{suffix}.log"),
        None => format!("{executable_name}.log"),
    };
    Some(std::env::current_dir().ok()?.join("logs").join(file_name))
}

#[cfg(test)]
mod logging_tests {
    use super::{create_file_log_writer_at, default_log_path};

    #[test]
    fn default_log_path_uses_working_directory_logs_folder() {
        let log_path = default_log_path(None).expect("log path should resolve");
        assert_eq!(
            log_path.parent().and_then(|parent| parent.file_name()),
            Some(std::ffi::OsStr::new("logs"))
        );
    }

    #[test]
    fn batch_log_path_uses_suffix() {
        let log_path = default_log_path(Some("batches")).expect("log path should resolve");
        assert!(log_path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains("-batches.log")));
    }

    #[test]
    fn file_log_initialization_returns_none_when_log_file_cannot_be_opened() {
        let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
        let log_path = temp_dir.path().join("jetstream-turbo.log");
        std::fs::create_dir(&log_path).expect("conflicting directory should be created");

        let writer = create_file_log_writer_at(&log_path);

        assert!(writer.is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::{handle_task_exit, panic_payload_to_string, Args, Command, ErrorReporter};
    use clap::Parser;
    use std::any::Any;

    #[test]
    fn panic_payload_to_string_supports_string() {
        let payload: Box<dyn Any + Send> = Box::new("panic message".to_string());
        assert_eq!(panic_payload_to_string(payload.as_ref()), "panic message");
    }

    #[test]
    fn panic_payload_to_string_supports_str() {
        let payload: Box<dyn Any + Send> = Box::new("panic message");
        assert_eq!(panic_payload_to_string(payload.as_ref()), "panic message");
    }

    #[test]
    fn panic_payload_to_string_handles_unknown_payload() {
        let payload: Box<dyn Any + Send> = Box::new(42_usize);
        assert_eq!(
            panic_payload_to_string(payload.as_ref()),
            "unknown panic payload"
        );
    }

    #[tokio::test]
    async fn handle_task_exit_handles_task_panic_path() {
        let reporter = ErrorReporter::new(None, None).await;
        let join_result = tokio::spawn(async move {
            panic!("simulated task panic");
        })
        .await;

        let task_name = handle_task_exit("turbocharger", join_result, &reporter);
        assert_eq!(task_name, "turbocharger");
    }

    #[test]
    fn schema_maintenance_subcommand_parses_without_serve_arguments() {
        let args = Args::try_parse_from([
            "jetstream-turbo",
            "schema-maintenance",
            "--busy-timeout-secs",
            "7",
        ])
        .unwrap();

        assert_eq!(
            args.command,
            Some(Command::SchemaMaintenance {
                busy_timeout_secs: Some(7)
            })
        );
    }
}
