use anyhow::Result;
use clap::Parser;
use jetstream_turbo_rs::config::Settings;
use jetstream_turbo_rs::server::create_server;
use jetstream_turbo_rs::telemetry::ErrorReporter;
use jetstream_turbo_rs::turbocharger::TurboCharger;
use std::any::Any;
use std::collections::HashMap;
use std::env;
use std::time::Duration;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

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

#[tokio::main]
async fn main() -> Result<()> {
    // Install rustls crypto provider
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|e| anyhow::anyhow!("Failed to install rustls crypto provider: {e:?}"))?;

    let args = Args::parse();

    // Default to warn in release mode, info in debug mode
    let log_level = args.log_level.unwrap_or_else(|| {
        if cfg!(debug_assertions) {
            "info".to_string()
        } else {
            "warn".to_string()
        }
    });

    // Initialize tracing
    init_tracing(&log_level)?;

    // Load configuration
    let settings = Settings::from_env()?;

    // Initialize error reporter
    let error_reporter = ErrorReporter::new(
        settings.posthog_api_key.clone(),
        settings.posthog_host.clone(),
    )
    .await;
    install_panic_hook(error_reporter.clone());

    tracing::info!("Starting jetstream-turbo v{}", env!("CARGO_PKG_VERSION"));
    tracing::info!(
        "Configuration loaded: modulo={}, shard={}",
        args.modulo,
        args.shard
    );

    // Create turbocharger
    let turbocharger = TurboCharger::new(
        settings.clone(),
        args.modulo,
        args.shard,
        error_reporter.clone(),
    )
    .await?;
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
        let restart_delay = Duration::from_secs(5);

        loop {
            match turbocharger_clone.run().await {
                Ok(()) => {
                    tracing::warn!("Turbocharger run loop ended unexpectedly; restarting");
                }
                Err(e) => {
                    tracing::error!("Turbocharger failed: {}", e);
                    let mut ctx = HashMap::new();
                    ctx.insert("component", "main");
                    ctx.insert("operation", "turbocharger_run");
                    error_reporter_clone.capture_error(&e, ctx);
                }
            }

            tracing::warn!(
                "Restarting turbocharger run loop in {} seconds",
                restart_delay.as_secs()
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

fn init_tracing(log_level: &str) -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().json())
        .init();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{handle_task_exit, panic_payload_to_string, ErrorReporter};
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
}
