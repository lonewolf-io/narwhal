// SPDX-License-Identifier: BSD-3-Clause

use std::future::Future;
use std::time::Duration;

#[cfg(feature = "runtime-monoio")]
mod monoio_impl {
  use super::*;

  /// A handle to a spawned task.
  pub type JoinHandle<T> = monoio::task::JoinHandle<T>;

  /// Spawns a task onto the current runtime.
  pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
  where
    F: Future + 'static,
    F::Output: 'static,
  {
    monoio::spawn(future)
  }

  /// Spawns a fire-and-forget task onto the current runtime.
  ///
  /// The task handle is intentionally dropped, detaching the task so it runs
  /// independently in the background.
  pub fn spawn_detached<F>(future: F)
  where
    F: Future + 'static,
    F::Output: 'static,
  {
    drop(monoio::spawn(future));
  }

  /// Sleeps for the specified duration.
  pub fn sleep(duration: Duration) -> impl Future<Output = ()> {
    monoio::time::sleep(duration)
  }

  /// Runs a future to completion with a timeout.
  pub async fn timeout<F>(duration: Duration, future: F) -> Result<F::Output, ()>
  where
    F: Future,
  {
    monoio::time::timeout(duration, future).await.map_err(|_| ())
  }

  /// Creates a new runtime instance and runs a future to completion on it.
  ///
  /// Returns an error if the runtime fails to initialize.
  pub fn try_block_on<F>(future: F) -> std::io::Result<F::Output>
  where
    F: Future,
  {
    let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new().enable_all().build()?;
    Ok(rt.block_on(future))
  }

  /// Creates a new runtime instance and runs a future to completion on it.
  ///
  /// # Panics
  ///
  /// Panics if the runtime fails to initialize.
  pub fn block_on<F>(future: F) -> F::Output
  where
    F: Future,
  {
    try_block_on(future).expect("failed to create runtime")
  }
}

#[cfg(not(any(feature = "runtime-monoio")))]
compile_error!("At least one runtime feature must be enabled for narwhal-common (e.g. 'runtime-monoio').");

#[cfg(feature = "runtime-monoio")]
pub use monoio_impl::*;
