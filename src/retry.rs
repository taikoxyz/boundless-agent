use std::future::Future;
use std::time::Duration;

use tokio::time::sleep;

use crate::boundless::{AgentError, AgentResult};

/// Retry helper with exponential backoff for transient operations.
pub async fn retry_with_backoff<F, Fut, T>(
    operation_name: &str,
    mut operation: F,
    max_attempts: u32,
) -> AgentResult<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = AgentResult<T>>,
{
    let mut attempt = 0;
    let mut delay = Duration::from_millis(500);

    loop {
        attempt += 1;
        match operation().await {
            Ok(result) => return Ok(result),
            Err(e) => {
                if attempt >= max_attempts {
                    tracing::error!(
                        "{} failed after {} attempts: {}",
                        operation_name,
                        attempt,
                        e
                    );
                    return Err(AgentError::RetryError(format!(
                        "{} failed after {} attempts: {}",
                        operation_name, attempt, e
                    )));
                }

                tracing::warn!(
                    "{} attempt {} failed: {}. Retrying in {:?}",
                    operation_name,
                    attempt,
                    e,
                    delay
                );
                sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(10));
            }
        }
    }
}

