use clap::Parser;
use std::env;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use jetstream_turbo::config::Settings;
use jetstream_turbo::turbocharger::TurboCharger;
use jetstream_turbo::server::create_server;
use anyhow::Result;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value_t = 0)]
    modulo: u32,
    
    #[arg(short, long, default_value_t = 0)]
    shard: u32,
    
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    
    // Initialize tracing
    init_tracing(&args.log_level)?;
    
    // Load configuration
    let settings = Settings::from_env()?;
    
    tracing::info!("Starting jetstream-turbo v{}", env!("CARGO_PKG_VERSION"));
    tracing::info!("Configuration loaded: modulo={}, shard={}", args.modulo, args.shard);
    
    // Create turbocharger
    let turbocharger = TurboCharger::new(settings.clone(), args.modulo, args.shard).await?;
    let turbocharger = std::sync::Arc::new(turbocharger);

    // Run both turbocharger and server
    let turbocharger_clone = turbocharger.clone();
    let turbocharger_handle = tokio::spawn(async move {
        if let Err(e) = turbocharger_clone.run().await {
            tracing::error!("Turbocharger failed: {}", e);
        }
    });

    let server_handle = tokio::spawn(async move {
        if let Err(e) = create_server(settings.http_port, turbocharger).await {
            tracing::error!("Server failed: {}", e);
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