// SPDX-License-Identifier: BSD-3-Clause

use std::future::Future;
use std::time::Duration;

use rand::RngExt;

/// Exponential backoff retry mechanism for async operations.
///
/// This struct provides a configurable exponential backoff strategy for retrying
/// failed async operations.
#[derive(Clone, Debug)]
pub struct ExponentialBackoff {
  initial_delay: Duration,
  max_delay: Duration,
  max_attempts: usize,
  jitter: bool,
  factor: f64,
}

impl Default for ExponentialBackoff {
  fn default() -> Self {
    Self {
      initial_delay: Duration::from_millis(150),
      max_delay: Duration::from_secs(60),
      max_attempts: 5,
      jitter: true,
      factor: 1.5,
    }
  }
}

impl ExponentialBackoff {
  /// Creates a new `ExponentialBackoff` with default configuration.
  pub fn new() -> Self {
    Self::default()
  }

  /// Sets the initial delay before the first retry.
  ///
  /// This is the base delay that will be multiplied by the factor
  /// for each subsequent retry attempt.
  ///
  /// # Arguments
  ///
  /// * `delay` - The initial delay duration
  pub fn with_initial_delay(mut self, delay: Duration) -> Self {
    self.initial_delay = delay;
    self
  }

  /// Sets the maximum delay between retry attempts.
  ///
  /// Even if the exponential backoff calculation would result in a longer
  /// delay, it will be capped at this maximum value.
  ///
  /// # Arguments
  ///
  /// * `delay` - The maximum delay duration
  pub fn with_max_delay(mut self, delay: Duration) -> Self {
    self.max_delay = delay;
    self
  }

  /// Sets the maximum number of retry attempts.
  ///
  /// The operation will be attempted at most this many times before giving up.
  ///
  /// # Arguments
  ///
  /// * `attempts` - The maximum number of attempts (must be > 0)
  ///
  /// # Panics
  ///
  /// Panics if `attempts` is 0.
  pub fn with_max_attempts(mut self, attempts: usize) -> Self {
    assert!(attempts > 0, "Max attempts must be greater than 0");
    self.max_attempts = attempts;
    self
  }

  /// Sets whether to apply jitter to the delay.
  ///
  /// If `true`, a random jitter will be added to the delay
  /// to avoid thundering herd problems.
  ///
  /// # Arguments
  ///
  /// * `jitter` - Whether to apply jitter
  pub fn with_jitter(mut self, jitter: bool) -> Self {
    self.jitter = jitter;
    self
  }

  /// Sets the exponential backoff factor.
  ///
  /// This multiplier is applied to the current delay to calculate the next delay.
  /// For example, with a factor of 2.0, delays will be: 150ms, 300ms, 600ms, 1200ms, etc.
  ///
  /// # Arguments
  ///
  /// * `factor` - The multiplier for exponential backoff (must be > 1.0)
  ///
  /// # Panics
  ///
  /// Panics if `factor` is not greater than 1.0.
  pub fn with_factor(mut self, factor: f64) -> Self {
    assert!(factor > 1.0, "Backoff factor must be greater than 1.0");
    self.factor = factor;
    self
  }

  /// Retries an async operation with exponential backoff.
  ///
  /// This method will attempt to execute the provided operation up to `max_attempts` times.
  /// If an operation fails, it will wait for the current delay period before retrying.
  /// The delay increases exponentially after each failure according to the configured factor.
  ///
  /// # Arguments
  ///
  /// * `operation` - A closure that returns a future resolving to `Result<T, E>`
  /// * `sleep` - A function that returns a future that resolves after the given duration,
  ///   provided by the caller so that the appropriate runtime's timer is used
  ///
  /// # Returns
  ///
  /// * `Ok(T)` - If any attempt succeeds
  /// * `Err(E)` - The error from the final attempt if all attempts fail
  ///
  /// # Type Parameters
  ///
  /// * `F` - The closure type that produces futures
  /// * `Fut` - The future type returned by the closure
  /// * `T` - The success type
  /// * `E` - The error type
  /// * `S` - The sleep function type
  /// * `SFut` - The future type returned by the sleep function
  pub async fn retry_with_backoff<F, Fut, T, E, S, SFut>(&self, mut operation: F, sleep: S) -> Result<T, E>
  where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    S: Fn(Duration) -> SFut,
    SFut: Future<Output = ()>,
  {
    let mut delay = self.initial_delay;
    let mut last_error: Option<E> = None;

    for _ in 0..self.max_attempts {
      match operation().await {
        Ok(result) => return Ok(result),
        Err(err) => {
          // Keep track of the last error
          last_error = Some(err);

          let actual_delay = if self.jitter {
            // Apply full jitter: randomize between 0 and the calculated delay
            let delay_ms = delay.as_millis() as u64;
            if delay_ms > 0 {
              let random_ms = rand::rng().random_range(..delay_ms);
              Duration::from_millis(delay_ms + random_ms)
            } else {
              delay
            }
          } else {
            delay
          };

          // Wait for the current delay before retrying
          sleep(actual_delay).await;

          // Calculate next delay with exponential backoff
          let next_delay = Duration::from_secs_f64(delay.as_secs_f64() * self.factor);
          delay = std::cmp::min(next_delay, self.max_delay);
        },
      }
    }

    // Safe unwrap since we know last_error is Some after the loop
    Err(last_error.unwrap())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::{Arc, Mutex};
  use std::time::Duration;
  use std::time::Instant;

  // Helper struct to control operation behavior
  #[derive(Clone)]
  struct MockOperation {
    call_count: Arc<Mutex<usize>>,
    fail_until_attempt: usize,
  }

  impl MockOperation {
    fn new(fail_until_attempt: usize) -> Self {
      Self { call_count: Arc::new(Mutex::new(0)), fail_until_attempt }
    }

    async fn call(&self) -> Result<String, &'static str> {
      let mut count = self.call_count.lock().unwrap();
      *count += 1;
      let current_attempt = *count;

      if current_attempt > self.fail_until_attempt {
        Ok(format!("success on attempt {}", current_attempt))
      } else {
        Err("operation failed")
      }
    }

    fn call_count(&self) -> usize {
      *self.call_count.lock().unwrap()
    }
  }

  #[test]
  fn test_default_configuration() {
    let backoff = ExponentialBackoff::default();
    assert!(backoff.jitter);
    assert_eq!(backoff.initial_delay, Duration::from_millis(150));
    assert_eq!(backoff.max_delay, Duration::from_secs(60));
    assert_eq!(backoff.max_attempts, 5);
    assert_eq!(backoff.factor, 1.5);
  }

  #[test]
  fn test_builder_pattern() {
    let backoff =
      ExponentialBackoff::new().with_initial_delay(Duration::from_millis(100)).with_max_attempts(3).with_factor(1.5);

    assert_eq!(backoff.initial_delay, Duration::from_millis(100));
    assert_eq!(backoff.max_attempts, 3);
    assert_eq!(backoff.factor, 1.5);
  }

  #[test]
  #[should_panic(expected = "Max attempts must be greater than 0")]
  fn test_zero_max_attempts_panics() {
    ExponentialBackoff::new().with_max_attempts(0);
  }

  #[test]
  #[should_panic(expected = "Backoff factor must be greater than 1.0")]
  fn test_invalid_factor_panics() {
    ExponentialBackoff::new().with_factor(1.0);
  }

  #[compio::test]
  async fn test_success_on_first_attempt() {
    let backoff = ExponentialBackoff::new();
    let mock_op = MockOperation::new(0); // Succeed immediately

    let result = backoff.retry_with_backoff(|| mock_op.call(), compio::runtime::time::sleep).await;

    assert!(result.is_ok());
    assert_eq!(mock_op.call_count(), 1);
  }

  #[compio::test]
  async fn test_success_after_retries() {
    let backoff = ExponentialBackoff::new().with_max_attempts(3).with_initial_delay(Duration::from_millis(10));

    let mock_op = MockOperation::new(2); // Fail twice, succeed on third

    let result = backoff.retry_with_backoff(|| mock_op.call(), compio::runtime::time::sleep).await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "success on attempt 3");
    assert_eq!(mock_op.call_count(), 3);
  }

  #[compio::test]
  async fn test_all_attempts_fail() {
    let backoff = ExponentialBackoff::new().with_max_attempts(3).with_initial_delay(Duration::from_millis(5));

    let mock_op = MockOperation::new(999); // Always fail

    let result = backoff.retry_with_backoff(|| mock_op.call(), compio::runtime::time::sleep).await;

    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "operation failed");
    assert_eq!(mock_op.call_count(), 3);
  }

  #[compio::test]
  async fn test_exponential_backoff_timing() {
    let backoff =
      ExponentialBackoff::new().with_max_attempts(3).with_initial_delay(Duration::from_millis(20)).with_jitter(false); // Deterministic timing

    let mock_op = MockOperation::new(999); // Always fail

    let start = Instant::now();
    let _result = backoff.retry_with_backoff(|| mock_op.call(), compio::runtime::time::sleep).await;
    let elapsed = start.elapsed();

    // Should wait: 20ms + 40ms = 60ms minimum (2 delays between 3 attempts)
    assert!(elapsed >= Duration::from_millis(60));
    assert_eq!(mock_op.call_count(), 3);
  }

  #[compio::test]
  async fn test_max_delay_cap() {
    let backoff = ExponentialBackoff::new()
            .with_max_attempts(3)
            .with_initial_delay(Duration::from_millis(100))
            .with_factor(10.0) // Very aggressive growth
            .with_max_delay(Duration::from_millis(150)) // Cap it
            .with_jitter(false);

    let mock_op = MockOperation::new(999); // Always fail

    let start = Instant::now();
    let _result = backoff.retry_with_backoff(|| mock_op.call(), compio::runtime::time::sleep).await;
    let elapsed = start.elapsed();

    assert!(elapsed >= Duration::from_millis(400));
    assert_eq!(mock_op.call_count(), 3);
  }
}
