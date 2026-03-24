// SPDX-License-Identifier: BSD-3-Clause

use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::anyhow;
use libc::{
  SIG_BLOCK, SIG_SETMASK, SIGHUP, SIGINT, SIGPIPE, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2, pthread_sigmask, sigaddset,
  sigemptyset, sigset_t,
};
use tracing::{error, info, trace, warn};

use crate::runtime;

/// Type alias for runtime task handles.
///
/// This represents a spawned task that can be awaited or detached.
pub type Task = crate::runtime::JoinHandle<()>;

type SpawnFn = Box<dyn FnOnce() + Send + 'static>;

struct WorkerHandle {
  thread_handle: thread::JoinHandle<anyhow::Result<()>>,
  worker_id: usize,
  shutdown_tx: async_channel::Sender<()>,
}

/// A dispatcher that owns N OS threads, each running a runtime pinned
/// to a CPU core. Work is dispatched to a specific shard's runtime via
/// `dispatch_at_shard`.
#[derive(Clone)]
pub struct CoreDispatcher {
  worker_count: usize,
  senders: Arc<Vec<async_channel::Sender<SpawnFn>>>,
  handles: Arc<Mutex<Vec<WorkerHandle>>>,
}

// === impl CoreDispatcher ===

impl CoreDispatcher {
  /// Creates a new `CoreDispatcher` with the given number of worker threads.
  pub fn new(worker_count: usize) -> Self {
    debug_assert!(worker_count > 0, "worker count must be greater than 0");

    Self { worker_count, senders: Arc::new(Vec::new()), handles: Arc::new(Mutex::new(Vec::new())) }
  }

  /// Spawns worker threads, each with a runtime pinned to a CPU core.
  ///
  /// Returns after all workers have signaled readiness.
  pub async fn bootstrap(&mut self) -> anyhow::Result<()> {
    let worker_count = self.worker_count;

    let core_ids = core_affinity::get_core_ids().unwrap_or_default();
    let available_cores = core_ids.len();

    if available_cores == 0 {
      warn!("no CPU cores detected, workers will run without core affinity");
    } else {
      trace!(worker_count, available_cores, "spawning core workers with CPU core affinity");
    }

    // Block signals before creating worker threads.
    let old_mask = block_signals();

    let mut senders = Vec::with_capacity(worker_count);
    let mut handles = Vec::with_capacity(worker_count);
    let mut ready_rxs = Vec::with_capacity(worker_count);

    for worker_id in 0..worker_count {
      let (task_tx, task_rx) = async_channel::unbounded::<SpawnFn>();
      let (shutdown_tx, shutdown_rx) = async_channel::bounded::<()>(1);
      let (ready_tx, ready_rx) = async_channel::bounded::<()>(1);

      ready_rxs.push(ready_rx);
      senders.push(task_tx);

      let core_id = if !core_ids.is_empty() { Some(core_ids[worker_id % core_ids.len()]) } else { None };

      let thread_handle =
        thread::Builder::new().name(format!("core-worker-{}", worker_id)).spawn(move || -> anyhow::Result<()> {
          trace!(worker_id, "core worker thread started");

          if let Some(core_id) = core_id {
            let _ = core_affinity::set_for_current(core_id);
          }

          runtime::try_block_on(async move {
            // Spawn a task that drains incoming work from the task channel.
            runtime::spawn_detached(async move {
              while let Ok(f) = task_rx.recv().await {
                f();
              }
            });

            // Signal readiness.
            let _ = ready_tx.send(()).await;

            // Keep the runtime alive until shutdown signal.
            let _ = shutdown_rx.recv().await;
          })
          .map_err(|e| anyhow!("failed to create runtime for worker {}: {}", worker_id, e))?;

          trace!(worker_id, "core worker thread stopped");

          std::result::Result::Ok(())
        })?;

      handles.push(WorkerHandle { thread_handle, worker_id, shutdown_tx });
    }

    // Restore the original signal mask.
    restore_signals(old_mask);

    // Wait for all workers to be ready.
    for rx in ready_rxs {
      let _ = rx.recv().await;
    }

    self.senders = Arc::new(senders);
    *self.handles.lock().unwrap() = handles;

    info!(worker_count, "core dispatcher started");

    Ok(())
  }

  /// Dispatches a future-producing closure to a specific shard's runtime.
  pub async fn dispatch_at_shard<F, Fut>(&self, shard: usize, f: F) -> anyhow::Result<()>
  where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
  {
    let task: SpawnFn = Box::new(move || {
      runtime::spawn_detached(f());
    });

    self.senders[shard].send(task).await.map_err(|_| anyhow!("shard {} channel closed", shard))
  }

  /// Returns the number of worker shards.
  pub fn shard_count(&self) -> usize {
    self.worker_count
  }

  /// Shuts down all worker threads gracefully.
  ///
  /// Closes task channels, sends shutdown signals, and joins all threads.
  pub async fn shutdown(&mut self) -> anyhow::Result<()> {
    // Close all task senders so no new work is accepted.
    for sender in self.senders.iter() {
      sender.close();
    }

    let handles = std::mem::take(&mut *self.handles.lock().unwrap());

    if handles.is_empty() {
      return Ok(());
    }

    trace!(worker_count = handles.len(), "shutting down core workers");

    // Signal all workers to shutdown.
    for handle in &handles {
      if let Err(e) = handle.shutdown_tx.send(()).await {
        warn!(worker_id = handle.worker_id, error = ?e, "failed to send shutdown signal to core worker");
      }
    }

    // Join all worker threads.
    for handle in handles {
      match handle.thread_handle.join() {
        std::result::Result::Ok(std::result::Result::Ok(())) => {
          trace!(worker_id = handle.worker_id, "core worker joined successfully");
        },
        std::result::Result::Ok(Err(e)) => {
          error!(worker_id = handle.worker_id, error = ?e, "core worker returned error");
        },
        Err(e) => {
          warn!(worker_id = handle.worker_id, error = ?e, "core worker panicked");
        },
      }
    }

    info!("core dispatcher stopped");

    Ok(())
  }
}

impl std::fmt::Debug for CoreDispatcher {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("CoreDispatcher").field("worker_count", &self.worker_count).finish()
  }
}

fn block_signals() -> sigset_t {
  let mut new_mask: sigset_t = unsafe { std::mem::zeroed() };
  unsafe {
    sigemptyset(&mut new_mask);
    sigaddset(&mut new_mask, SIGINT);
    sigaddset(&mut new_mask, SIGTERM);
    sigaddset(&mut new_mask, SIGQUIT);
    sigaddset(&mut new_mask, SIGHUP);
    sigaddset(&mut new_mask, SIGUSR1);
    sigaddset(&mut new_mask, SIGUSR2);
    sigaddset(&mut new_mask, SIGPIPE);
  }

  let mut old_mask: sigset_t = unsafe { std::mem::zeroed() };
  unsafe {
    pthread_sigmask(SIG_BLOCK, &new_mask, &mut old_mask);
  }

  old_mask
}

fn restore_signals(old_mask: sigset_t) {
  unsafe {
    pthread_sigmask(SIG_SETMASK, &old_mask, std::ptr::null_mut());
  }
}
