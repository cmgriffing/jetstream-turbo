use anyhow::Result;
use clap::{Parser, Subcommand};
use jetstream_turbo_rs::config::Settings;
use jetstream_turbo_rs::server::create_server;
use jetstream_turbo_rs::storage::{SQLitePragmaConfig, SQLiteStore};
use jetstream_turbo_rs::telemetry::ErrorReporter;
use jetstream_turbo_rs::turbocharger::ProductionTurboCharger as TurboCharger;
use jetstream_turbo_rs::utils::logging::retention::{LogOwner, LogPolicy};
use std::any::Any;
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::time::Duration;
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
    let bootstrap_settings = Settings::from_env_for_schema_maintenance()?;
    // SQLite reads this process-wide setting when opening its first temp file.
    // Set it once, before schema checks, pools or ingestion tasks are started.
    let vacuum_temp = bootstrap_settings
        .vacuum_temp_dir
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("SQLITE_TMPDIR")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
        });
    std::env::set_var("SQLITE_TMPDIR", &vacuum_temp);
    let _log_guards = init_tracing(&log_level, &bootstrap_settings, command.is_none())?;

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

    // Independent of the ingestion loop so SQLite stalls cannot suppress samples.
    turbocharger.start_memory_observer_task();

    // Start background session refresh task
    turbocharger.start_session_refresh_task();

    // Start background database cleanup task
    turbocharger.start_db_cleanup_task();

    // Run both turbocharger and server
    let turbocharger_clone = turbocharger.clone();
    let error_reporter_clone = error_reporter.clone();
    let memory_emergency_exit_enabled = settings.memory_emergency_exit_enabled;
    let turbocharger_handle = tokio::spawn(async move {
        loop {
            let restart_delay = match turbocharger_clone.run().await {
                Ok(()) => {
                    tracing::warn!("Turbocharger run loop ended unexpectedly; restarting");
                    turbocharger_clone.minimum_recovery_delay()
                }
                Err(failure) => {
                    if matches!(
                        failure.error(),
                        jetstream_turbo_rs::TurboError::ControlledMemoryExit
                    ) && memory_emergency_exit_enabled
                    {
                        tracing::error!(
                            exit_code = 75,
                            "Exiting after controlled runtime-memory containment"
                        );
                        std::process::exit(75);
                    }
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

fn init_tracing(
    log_level: &str,
    settings: &Settings,
    file_logging_enabled: bool,
) -> Result<Option<LogOwner>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level));
    let stdout_layer = tracing_subscriber::fmt::layer().json();
    // Maintenance can run beside the service. Leave its managed files and
    // ownership lock untouched, and report maintenance output to the caller.
    if !file_logging_enabled {
        tracing_subscriber::registry()
            .with(filter)
            .with(stdout_layer)
            .init();
        return Ok(None);
    }
    let owner = default_log_path(None).and_then(|path| {
        let policy = LogPolicy {
            max_segment_bytes: settings.log_segment_max_mb.saturating_mul(1024 * 1024),
            max_total_bytes: settings.log_total_max_mb.saturating_mul(1024 * 1024),
            retention_seconds: settings.log_retention_seconds,
            queue_capacity: settings.log_queue_capacity,
            max_record_bytes: settings.log_max_record_bytes,
            cleanup_interval: Duration::from_secs(60),
        };
        match LogOwner::start(path.parent()?, policy) {
            Ok(owner) => Some(owner),
            Err(error) => {
                eprintln!("Application file logging unavailable: {error}");
                None
            }
        }
    });
    if let Some(owner) = owner {
        let main_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(owner.main_writer())
            .with_filter(filter_fn(|metadata| {
                metadata.target() != BATCH_REPORT_LOG_TARGET
            }));
        let batch_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(owner.batch_writer())
            .with_filter(filter_fn(|metadata| {
                metadata.target() == BATCH_REPORT_LOG_TARGET
            }));
        tracing_subscriber::registry()
            .with(filter)
            .with(stdout_layer)
            .with(main_layer)
            .with(batch_layer)
            .init();
        Ok(Some(owner))
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(stdout_layer)
            .init();
        Ok(None)
    }
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
    use super::{default_log_path, LogOwner, LogPolicy};

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
        std::fs::write(&log_path, b"conflicting file").expect("conflicting file should be created");

        let writer = LogOwner::start(&log_path, LogPolicy::default());

        assert!(writer.is_err());
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
