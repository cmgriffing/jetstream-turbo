pub mod client;
pub mod config;
pub mod hydration;
pub mod models;
pub mod server;
pub mod storage;
pub mod telemetry;
pub mod turbocharger;
pub mod utils;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

// Re-export main types for convenience
pub use config::Settings;
pub use models::errors::TurboError;
pub use telemetry::ErrorReporter;
pub use turbocharger::{ProductionTurboCharger, TurboCharger};
