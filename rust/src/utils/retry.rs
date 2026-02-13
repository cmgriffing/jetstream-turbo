use crate::models::errors::{TurboError, TurboResult};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, warn};

/// Retry configuration
#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub backoff_multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(30),
            backoff_multiplier: 2.0,
        }
    }
}

/// Retry with exponential backoff
pub async fn retry_with_backoff<F, T, E>(config: RetryConfig, mut operation: F) -> TurboResult<T>
where
    F: FnMut() -> Result<T, E>,
    E: std::fmt::Display,
{
    let mut last_error = None;

    for attempt in 1..=config.max_attempts {
        match operation() {
            Ok(result) => {
                if attempt > 1 {
                    debug!("Operation succeeded on attempt {}", attempt);
                }
                return Ok(result);
            }
            Err(e) => {
                warn!("Operation failed on attempt {}: {}", attempt, e);
                last_error = Some(format!("{e}"));

                if attempt < config.max_attempts {
                    let delay = calculate_backoff_delay(attempt - 1, &config);
                    debug!("Retrying in {:?} (attempt {})", delay, attempt);
                    sleep(delay).await;
                }
            }
        }
    }

    Err(TurboError::Internal(format!(
        "Operation failed after {} attempts: {}",
        config.max_attempts,
        last_error.unwrap_or_default()
    )))
}

/// Calculate exponential backoff delay
fn calculate_backoff_delay(attempt: u32, config: &RetryConfig) -> Duration {
    let delay_ms =
        (config.base_delay.as_millis() as f64) * config.backoff_multiplier.powi(attempt as i32);

    let delay_ms = delay_ms.min(config.max_delay.as_millis() as f64) as u64;
    Duration::from_millis(delay_ms)
}

/// Simple retry without backoff (immediate retry)
#[allow(unused_mut)]
pub async fn retry_immediate<F, T, E>(max_attempts: u32, mut operation: F) -> TurboResult<T>
where
    F: FnMut() -> Result<T, E>,
    E: std::fmt::Display,
{
    let config = RetryConfig {
        max_attempts,
        base_delay: Duration::from_millis(1),
        ..Default::default()
    };

    retry_with_backoff(config, operation).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_retry_success_on_first_attempt() {
        let mut call_count = 0;
        let result = retry_with_backoff(RetryConfig::default(), || {
            call_count += 1;
            Ok("success")
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "success");
        assert_eq!(call_count, 1);
    }

    #[tokio::test]
    async fn test_retry_success_on_later_attempt() {
        let mut call_count = 0;
        let result = retry_with_backoff(RetryConfig::default(), || {
            call_count += 1;
            if call_count < 3 {
                Err("temporary failure")
            } else {
                Ok("success")
            }
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "success");
        assert_eq!(call_count, 3);
    }

    #[tokio::test]
    async fn test_retry_failure_after_max_attempts() {
        let mut call_count = 0;
        let result = retry_with_backoff(
            RetryConfig {
                max_attempts: 2,
                ..Default::default()
            },
            || {
                call_count += 1;
                Err("persistent failure")
            },
        )
        .await;

        assert!(result.is_err());
        assert_eq!(call_count, 2);
    }

    #[test]
    fn test_backoff_calculation() {
        let config = RetryConfig {
            base_delay: Duration::from_millis(100),
            backoff_multiplier: 2.0,
            max_delay: Duration::from_millis(1000),
            ..Default::default()
        };

        // Attempt 0 -> 100ms
        let delay = calculate_backoff_delay(0, &config);
        assert_eq!(delay, Duration::from_millis(100));

        // Attempt 1 -> 200ms
        let delay = calculate_backoff_delay(1, &config);
        assert_eq!(delay, Duration::from_millis(200));

        // Attempt 2 -> 400ms
        let delay = calculate_backoff_delay(2, &config);
        assert_eq!(delay, Duration::from_millis(400));

        // Attempt with max delay limit
        let config_max = RetryConfig {
            base_delay: Duration::from_millis(100),
            backoff_multiplier: 20.0,
            max_delay: Duration::from_millis(500),
            ..Default::default()
        };

        let delay = calculate_backoff_delay(3, &config_max);
        assert_eq!(delay, Duration::from_millis(500)); // Should cap at max
    }
}
