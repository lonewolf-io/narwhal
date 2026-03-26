// SPDX-License-Identifier: BSD-3-Clause

use std::future::Future;
use std::time::Duration;

use std::pin::Pin;
use std::task::{Context, Poll};

/// A handle to a spawned task.
///
/// Wraps compio's `JoinHandle` so that awaiting it yields `T` directly
/// (propagating panics) rather than `Result<T, Box<dyn Any + Send>>`.
pub struct JoinHandle<T>(compio::runtime::JoinHandle<T>);

impl<T> Future for JoinHandle<T> {
  type Output = T;

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
    // SAFETY: JoinHandle is pinned, inner field is structurally pinned.
    let inner = unsafe { self.map_unchecked_mut(|s| &mut s.0) };
    match inner.poll(cx) {
      Poll::Ready(Ok(val)) => Poll::Ready(val),
      Poll::Ready(Err(e)) => std::panic::resume_unwind(e),
      Poll::Pending => Poll::Pending,
    }
  }
}

/// Spawns a task onto the current runtime.
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
  F: Future + 'static,
  F::Output: 'static,
{
  JoinHandle(compio::runtime::spawn(future))
}

/// Spawns a fire-and-forget task onto the current runtime.
///
/// In compio, dropping a task handle cancels the task, so we must
/// explicitly detach it.
pub fn spawn_detached<F>(future: F)
where
  F: Future + 'static,
  F::Output: 'static,
{
  compio::runtime::spawn(future).detach();
}

/// Sleeps for the specified duration.
pub fn sleep(duration: Duration) -> impl Future<Output = ()> {
  compio::runtime::time::sleep(duration)
}

/// Runs a future to completion with a timeout.
pub async fn timeout<F>(duration: Duration, future: F) -> Result<F::Output, ()>
where
  F: Future,
{
  compio::runtime::time::timeout(duration, future).await.map_err(|_| ())
}

/// Creates a new runtime instance and runs a future to completion on it.
///
/// Returns an error if the runtime fails to initialize.
pub fn try_block_on<F>(future: F) -> std::io::Result<F::Output>
where
  F: Future,
{
  let rt = compio::runtime::Runtime::new()?;
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

pub use compio::net::TcpListener;
pub use compio::net::TcpStream;
pub use compio::net::UnixListener;
pub use compio::net::UnixStream;

pub use compio::io::AsyncWrite;
pub use compio::io::AsyncWriteExt;
