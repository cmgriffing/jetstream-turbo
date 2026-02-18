use anyhow::Result;
use clap::Parser;
use jetstream_turbo_rs::config::Settings;
use jetstream_turbo_rs::server::create_server;
use jetstream_turbo_rs::telemetry::ErrorReporter;
use jetstream_turbo_rs::turbocharger::TurboCharger;
use std::collections::HashMap;
use std::env;
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
        .expect("Failed to install rustls crypto provider");

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
    ).await;

    tracing::info!("Starting jetstream-turbo v{}", env!("CARGO_PKG_VERSION"));
    tracing::info!(
        "Configuration loaded: modulo={}, shard={}",
        args.modulo,
        args.shard
    );

    // Create turbocharger
    let turbocharger = TurboCharger::new(settings.clone(), args.modulo, args.shard, error_reporter.clone()).await?;
    let turbocharger = std::sync::Arc::new(turbocharger);

    // Start background session refresh task
    turbocharger.start_session_refresh_task();

    // Run both turbocharger and server
    let turbocharger_clone = turbocharger.clone();
    let error_reporter_clone = error_reporter.clone();
    let turbocharger_handle = tokio::spawn(async move {
        if let Err(e) = turbocharger_clone.run().await {
            tracing::error!("Turbocharger failed: {}", e);
            let mut ctx = HashMap::new();
            ctx.insert("component", "main");
            ctx.insert("operation", "turbocharger_run");
            error_reporter_clone.capture_error(&e, ctx);
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

    // Wait for either task to complete
    tokio::select! {
        _ = turbocharger_handle => {
            tracing::info!("Turbocharger task completed");
        }
        _ = server_handle => {
            tracing::info!("Server task completed");
        }
    }

    Ok(())
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
