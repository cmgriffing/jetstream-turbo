use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Initialize structured logging for the application
pub fn init_tracing(log_level: &str) -> Result<(), Box<dyn std::error::Error>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().json())
        .init();

    info!("Logging initialized with level: {}", log_level);
    Ok(())
}

/// Initialize tracing for testing
#[cfg(test)]
pub fn init_test_tracing() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .init();
}

/// Macro for logging errors with context
#[macro_export]
macro_rules! log_error {
    ($error:expr, $($arg:tt)*) => {
        error!("{}: {}", $error, format!($($arg)*));
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_tracing() {
        // This test just ensures the function compiles
        let result = init_tracing("info");
        assert!(result.is_ok());
    }

    #[test]
    fn test_log_error_macro() {
        init_test_tracing();

        // Test the macro - this would normally log to stderr
        log_error!("Test error", "additional context: {}", "test");
    }
}
